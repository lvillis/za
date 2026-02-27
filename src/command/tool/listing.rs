use super::*;

#[derive(Debug, Clone)]
pub(super) enum LatestCheck {
    Latest(String),
    Unsupported,
    Error(String),
}

pub(super) fn list_update_status(installed_version: &str, latest: &LatestCheck) -> String {
    match latest {
        LatestCheck::Latest(remote)
            if normalize_version(installed_version) == normalize_version(remote) =>
        {
            "latest".to_string()
        }
        LatestCheck::Latest(remote) => format!("update -> {remote}"),
        LatestCheck::Unsupported => "n/a".to_string(),
        LatestCheck::Error(_) => "check-failed".to_string(),
    }
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct UnmanagedBinary {
    pub(super) name: String,
    pub(super) version: String,
    pub(super) path: String,
}

#[derive(Debug, Clone)]
struct ToolListRow {
    name: String,
    version: String,
    active: bool,
    source: String,
    update: Option<String>,
}

#[derive(Debug)]
struct ToolListReport {
    rows: Vec<ToolListRow>,
    unmanaged: Vec<UnmanagedBinary>,
    check_failures: Vec<(String, String)>,
    scope: String,
    bin_path: String,
    has_updates: bool,
}

#[derive(Debug, Serialize)]
struct ToolListJsonReport {
    scope: String,
    tool_binaries_path: String,
    rows: Vec<ToolListRowJson>,
    unmanaged: Vec<UnmanagedBinary>,
    update_check_warnings: Vec<String>,
    has_updates: bool,
    has_check_failures: bool,
}

#[derive(Debug, Serialize)]
struct ToolListRowJson {
    name: String,
    version: String,
    active: bool,
    source: String,
    update: Option<String>,
}

#[derive(Debug, Serialize)]
struct SupportedToolView {
    tool: String,
    aliases: Vec<String>,
    sources: String,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct ToolUpdateCacheFile {
    schema_version: u32,
    #[serde(default)]
    latest_versions: HashMap<String, ToolUpdateCacheEntry>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ToolUpdateCacheEntry {
    latest_version: String,
    fetched_at_unix_secs: u64,
}

#[derive(Debug)]
struct ToolUpdateCacheState {
    path: Option<PathBuf>,
    data: ToolUpdateCacheFile,
    dirty: bool,
}

impl ToolUpdateCacheState {
    fn load() -> Self {
        let path = tool_update_cache_path();
        let Some(path_ref) = path.as_ref() else {
            return Self {
                path,
                data: ToolUpdateCacheFile {
                    schema_version: TOOL_UPDATE_CACHE_SCHEMA_VERSION,
                    latest_versions: HashMap::new(),
                },
                dirty: false,
            };
        };
        match fs::read(path_ref) {
            Ok(raw) => match serde_json::from_slice::<ToolUpdateCacheFile>(&raw) {
                Ok(mut data) => {
                    if data.schema_version != TOOL_UPDATE_CACHE_SCHEMA_VERSION {
                        data = ToolUpdateCacheFile {
                            schema_version: TOOL_UPDATE_CACHE_SCHEMA_VERSION,
                            latest_versions: HashMap::new(),
                        };
                    }
                    Self {
                        path,
                        data,
                        dirty: false,
                    }
                }
                Err(_) => Self {
                    path,
                    data: ToolUpdateCacheFile {
                        schema_version: TOOL_UPDATE_CACHE_SCHEMA_VERSION,
                        latest_versions: HashMap::new(),
                    },
                    dirty: false,
                },
            },
            Err(_) => Self {
                path,
                data: ToolUpdateCacheFile {
                    schema_version: TOOL_UPDATE_CACHE_SCHEMA_VERSION,
                    latest_versions: HashMap::new(),
                },
                dirty: false,
            },
        }
    }

    fn get_latest_if_fresh(&mut self, key: &str, now_unix_secs: u64) -> Option<String> {
        if let Some(entry) = self.data.latest_versions.get(key) {
            if now_unix_secs.saturating_sub(entry.fetched_at_unix_secs)
                <= TOOL_UPDATE_CACHE_TTL_SECS
            {
                return Some(entry.latest_version.clone());
            }
            self.data.latest_versions.remove(key);
            self.dirty = true;
        }
        None
    }

    fn put_latest(&mut self, key: &str, latest_version: String, now_unix_secs: u64) {
        self.data.latest_versions.insert(
            key.to_string(),
            ToolUpdateCacheEntry {
                latest_version,
                fetched_at_unix_secs: now_unix_secs,
            },
        );
        self.dirty = true;
    }

    fn save_if_dirty(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let Some(path) = self.path.as_ref() else {
            return Ok(());
        };
        let Some(parent) = path.parent() else {
            return Ok(());
        };

        fs::create_dir_all(parent)
            .with_context(|| format!("create tool cache directory {}", parent.display()))?;
        let json = serde_json::to_vec_pretty(&self.data).context("serialize tool update cache")?;
        let tmp = path.with_extension(format!("tmp-tool-cache-{}", std::process::id()));
        fs::write(&tmp, json).with_context(|| format!("write {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| {
            format!("replace tool cache {} -> {}", path.display(), tmp.display())
        })?;
        self.dirty = false;
        Ok(())
    }
}

#[derive(Debug)]
struct LatestLookup {
    latest_by_name: HashMap<String, LatestCheck>,
}

pub(super) fn list(
    home: &ToolHome,
    supported_only: bool,
    check_updates: bool,
    json: bool,
    fail_on_updates: bool,
    fail_on_check_errors: bool,
) -> Result<i32> {
    if supported_only && check_updates {
        bail!("`--supported` cannot be combined with `--updates`");
    }
    if supported_only && (fail_on_updates || fail_on_check_errors) {
        bail!("`--fail-on-updates`/`--fail-on-check-errors` require `--updates`");
    }
    if !check_updates && (fail_on_updates || fail_on_check_errors) {
        bail!("`--fail-on-updates`/`--fail-on-check-errors` require `--updates`");
    }

    if supported_only {
        let rows = supported_tools_view();
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&rows).context("serialize supported tools JSON")?
            );
        } else {
            print_supported_tools(&rows);
        }
        return Ok(0);
    }

    let report = build_tool_list_report(home, check_updates)?;
    if json {
        print_tool_list_json(&report)?;
    } else {
        print_tool_list_text(&report, check_updates);
    }

    if fail_on_check_errors && !report.check_failures.is_empty() {
        eprintln!(
            "tool list policy failure: {} update checks failed",
            report.check_failures.len()
        );
        return Ok(TOOL_EXIT_UPDATE_CHECK_FAILED);
    }
    if fail_on_updates && report.has_updates {
        eprintln!("tool list policy failure: updates available");
        return Ok(TOOL_EXIT_UPDATES_AVAILABLE);
    }
    Ok(0)
}

fn build_tool_list_report(home: &ToolHome, check_updates: bool) -> Result<ToolListReport> {
    let mut rows = Vec::new();
    let mut name_entries = collect_dir_names(&home.store_dir)?;
    name_entries.sort();
    let latest_lookup = if check_updates {
        resolve_latest_checks_for_names(&name_entries)?
    } else {
        LatestLookup {
            latest_by_name: HashMap::new(),
        }
    };
    let unmanaged = collect_unmanaged_binaries(home)?;
    let mut check_failures: Vec<(String, String)> = Vec::new();
    let mut has_updates = false;

    for name in name_entries {
        let current = read_current_version(home, &name)?;
        let mut versions = collect_dir_names(&home.name_dir(&name))?;
        versions.sort();
        let latest = latest_lookup.latest_by_name.get(&name).cloned();
        if let Some(LatestCheck::Error(err)) = latest.as_ref() {
            check_failures.push((name.clone(), source::truncate_for_log(err, 120)));
        }

        for version in versions {
            let tool = ToolRef {
                name: name.clone(),
                version: version.clone(),
            };
            let is_current = current.as_deref() == Some(version.as_str());
            let source = manifest_source_label(home, &tool)?;
            let (update, update_available) = if let Some(latest) = latest.as_ref() {
                let status = list_update_status(&version, latest);
                let available = matches!(latest, LatestCheck::Latest(remote) if normalize_version(&version) != normalize_version(remote));
                (Some(status), available)
            } else {
                (None, false)
            };
            if update_available {
                has_updates = true;
            }
            rows.push(ToolListRow {
                name: name.clone(),
                version,
                active: is_current,
                source,
                update,
            });
        }
    }

    Ok(ToolListReport {
        rows,
        unmanaged,
        check_failures,
        scope: home.scope.label().to_string(),
        bin_path: home.bin_dir.display().to_string(),
        has_updates,
    })
}

fn print_tool_list_text(report: &ToolListReport, check_updates: bool) {
    if report.rows.is_empty() {
        println!("No tools installed.");
    } else if check_updates {
        println!(
            "{:<24} {:<20} {:<6} {:<18} SOURCE",
            "NAME", "VERSION", "ACTIVE", "UPDATE"
        );
        for row in &report.rows {
            let marker = if row.active { "*" } else { "" };
            let update = row.update.clone().unwrap_or_else(|| "n/a".to_string());
            println!(
                "{:<24} {:<20} {:<6} {:<18} {}",
                row.name, row.version, marker, update, row.source
            );
        }
    } else {
        println!("{:<24} {:<20} {:<6} SOURCE", "NAME", "VERSION", "ACTIVE");
        for row in &report.rows {
            let marker = if row.active { "*" } else { "" };
            println!(
                "{:<24} {:<20} {:<6} {}",
                row.name, row.version, marker, row.source
            );
        }
    }

    println!("\nScope: {}", report.scope);
    println!("Tool binaries path: {}", report.bin_path);
    if check_updates && !report.check_failures.is_empty() {
        println!("\nUpdate check warnings:");
        for (name, err) in &report.check_failures {
            println!("- {name}: {err}");
        }
    }
    print_unmanaged_binaries_text(&report.unmanaged);
}

fn print_tool_list_json(report: &ToolListReport) -> Result<()> {
    let json = ToolListJsonReport {
        scope: report.scope.clone(),
        tool_binaries_path: report.bin_path.clone(),
        rows: report
            .rows
            .iter()
            .map(|row| ToolListRowJson {
                name: row.name.clone(),
                version: row.version.clone(),
                active: row.active,
                source: row.source.clone(),
                update: row.update.clone(),
            })
            .collect(),
        unmanaged: report.unmanaged.clone(),
        update_check_warnings: report
            .check_failures
            .iter()
            .map(|(name, err)| format!("{name}: {err}"))
            .collect(),
        has_updates: report.has_updates,
        has_check_failures: !report.check_failures.is_empty(),
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&json).context("serialize tool list JSON")?
    );
    Ok(())
}

fn supported_tools_view() -> Vec<SupportedToolView> {
    tool_policies()
        .iter()
        .map(|policy| SupportedToolView {
            tool: policy.canonical_name.to_string(),
            aliases: policy
                .aliases
                .iter()
                .map(|alias| (*alias).to_string())
                .collect(),
            sources: policy.source_label.to_string(),
        })
        .collect()
}

fn print_supported_tools(rows: &[SupportedToolView]) {
    println!("{:<24} SOURCES", "TOOL");
    for row in rows {
        let tool_display = if row.aliases.is_empty() {
            row.tool.clone()
        } else {
            format!("{} / {}", row.tool, row.aliases.join(" / "))
        };
        println!("{:<24} {}", tool_display, row.sources);
    }
}

fn print_unmanaged_binaries_text(unmanaged: &[UnmanagedBinary]) {
    if unmanaged.is_empty() {
        return;
    }
    for item in unmanaged {
        println!(
            "\nDetected unmanaged binary: {} {} ({})",
            item.name, item.version, item.path
        );
        println!(
            "Run `za tool install {}` to adopt it into managed store.",
            item.name
        );
    }
}

fn resolve_latest_checks_for_names(names: &[String]) -> Result<LatestLookup> {
    let mut latest_by_name: HashMap<String, LatestCheck> = HashMap::new();
    let mut policy_tasks = Vec::new();
    let mut policy_seen: HashMap<&'static str, ()> = HashMap::new();

    let mut cache = ToolUpdateCacheState::load();
    let now_unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    for name in names {
        let Some(policy) = find_tool_policy(name) else {
            latest_by_name.insert(name.clone(), LatestCheck::Unsupported);
            continue;
        };
        if policy_seen.contains_key(policy.canonical_name) {
            continue;
        }
        policy_seen.insert(policy.canonical_name, ());
        if let Some(latest) = cache.get_latest_if_fresh(policy.canonical_name, now_unix_secs) {
            latest_by_name.insert(
                policy.canonical_name.to_string(),
                LatestCheck::Latest(latest),
            );
        } else {
            policy_tasks.push(policy);
        }
    }

    if !policy_tasks.is_empty() {
        let fetched = fetch_latest_checks_parallel(policy_tasks);
        for (canonical_name, latest_check) in fetched {
            if let LatestCheck::Latest(version) = &latest_check {
                cache.put_latest(canonical_name, version.clone(), now_unix_secs);
            }
            latest_by_name.insert(canonical_name.to_string(), latest_check);
        }
    }

    if let Err(err) = cache.save_if_dirty()
        && !is_permission_denied_error(&err)
    {
        eprintln!("warning: failed to persist tool update cache: {err}");
    }

    let mut by_name = HashMap::new();
    for name in names {
        let Some(policy) = find_tool_policy(name) else {
            by_name.insert(name.clone(), LatestCheck::Unsupported);
            continue;
        };
        let latest = latest_by_name
            .get(policy.canonical_name)
            .cloned()
            .unwrap_or(LatestCheck::Unsupported);
        by_name.insert(name.clone(), latest);
    }

    Ok(LatestLookup {
        latest_by_name: by_name,
    })
}

fn fetch_latest_checks_parallel(policies: Vec<ToolPolicy>) -> HashMap<&'static str, LatestCheck> {
    let worker_count = normalize_tool_update_jobs(default_tool_update_jobs(), policies.len());
    let queue = Arc::new(Mutex::new(VecDeque::from(policies)));
    let out: Arc<Mutex<HashMap<&'static str, LatestCheck>>> = Arc::new(Mutex::new(HashMap::new()));

    thread::scope(|scope| {
        for _ in 0..worker_count {
            let queue = Arc::clone(&queue);
            let out = Arc::clone(&out);
            scope.spawn(move || {
                loop {
                    let task = match queue.lock() {
                        Ok(mut guard) => guard.pop_front(),
                        Err(_) => None,
                    };
                    let Some(policy) = task else {
                        break;
                    };
                    let latest = resolve_latest_for_policy(policy);
                    if let Ok(mut guard) = out.lock() {
                        guard.insert(policy.canonical_name, latest);
                    } else {
                        break;
                    }
                }
            });
        }
    });

    out.lock()
        .map(|guard| guard.clone())
        .unwrap_or_else(|_| HashMap::new())
}

fn resolve_latest_for_policy(policy: ToolPolicy) -> LatestCheck {
    let Some(release) = policy.github_release else {
        return LatestCheck::Unsupported;
    };
    match source::fetch_latest_version_from_github_release(release) {
        Ok(version) => LatestCheck::Latest(version),
        Err(err) => LatestCheck::Error(format!("{err:#}")),
    }
}

fn normalize_tool_update_jobs(requested_jobs: usize, task_count: usize) -> usize {
    requested_jobs.max(1).min(task_count.max(1))
}

fn default_tool_update_jobs() -> usize {
    let cpus = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(TOOL_UPDATE_JOBS_MIN);
    cpus.saturating_mul(TOOL_UPDATE_JOBS_MULTIPLIER)
        .clamp(TOOL_UPDATE_JOBS_MIN, TOOL_UPDATE_JOBS_MAX)
}

fn tool_update_cache_path() -> Option<PathBuf> {
    if let Some(path) = env::var_os("XDG_CACHE_HOME").map(PathBuf::from) {
        return Some(path.join("za").join(TOOL_UPDATE_CACHE_FILE_NAME));
    }
    env::var_os("HOME").map(PathBuf::from).map(|home| {
        home.join(".cache")
            .join("za")
            .join(TOOL_UPDATE_CACHE_FILE_NAME)
    })
}
