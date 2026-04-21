use super::*;
use std::fmt::Write as _;

const CODEX_PS_CACHE_RELATIVE: &str = "za/codex/ps-cache.json";
const CODEX_PS_CACHE_VERSION: u32 = 1;

#[derive(Debug)]
pub(super) struct WorkspaceContext {
    pub(super) workspace_root: PathBuf,
    pub(super) workspace_label: String,
    pub(super) workspace_hash: String,
    pub(super) session_name: String,
    pub(super) metadata_path: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct SessionRecord {
    pub(super) session_name: String,
    pub(super) workspace_root: String,
    pub(super) workspace_label: String,
    pub(super) workspace_hash: String,
    pub(super) created_at_unix: u64,
    pub(super) launcher: String,
    pub(super) launcher_args: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct CodexSessionRow {
    pub(super) session_name: String,
    pub(super) status: String,
    pub(super) attached_clients: usize,
    pub(super) last_activity_unix: Option<u64>,
    pub(super) created_unix: Option<u64>,
    pub(super) codex_session_id: Option<String>,
    pub(super) codex_model: Option<String>,
    pub(super) codex_effort: Option<String>,
    pub(super) context_left_percent: Option<f64>,
    pub(super) workspace_root: Option<String>,
    pub(super) workspace_label: Option<String>,
    pub(super) metadata_present: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct CodexPsOutput {
    pub(super) tmux_available: bool,
    pub(super) sessions: Vec<CodexSessionRow>,
}

#[derive(Debug, Serialize)]
pub(super) struct CodexStopOutput {
    pub(super) session_name: String,
    pub(super) workspace_root: String,
    pub(super) stopped: bool,
    pub(super) metadata_removed: bool,
    pub(super) tmux_available: bool,
    pub(super) note: Option<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct CodexStopAllOutput {
    pub(super) tmux_available: bool,
    pub(super) sessions: Vec<CodexStopOutput>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct CodexSessionSummary {
    pub(super) session_id: String,
    pub(super) workspace_root: String,
    pub(super) modified_unix: u64,
    pub(super) model: Option<String>,
    pub(super) effort: Option<String>,
    pub(super) context_left_percent: Option<f64>,
}

#[derive(Clone, Debug)]
pub(super) struct CodexPsScanPaths {
    pub(super) sessions_root: PathBuf,
    pub(super) legacy_log_path: PathBuf,
    pub(super) cache_paths: Vec<PathBuf>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(super) struct CodexPsCache {
    #[serde(default)]
    pub(super) version: u32,
    #[serde(default)]
    pub(super) session_files: BTreeMap<String, CachedSessionSummaryEntry>,
    #[serde(default)]
    pub(super) legacy_log: Option<LegacyContextCache>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct CachedSessionSummaryEntry {
    pub(super) len: u64,
    pub(super) modified_unix: u64,
    pub(super) summary: Option<CodexSessionSummary>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct LegacyContextCache {
    pub(super) len: u64,
    pub(super) modified_unix: u64,
    pub(super) values: BTreeMap<String, f64>,
}

#[derive(Debug, Deserialize)]
struct SessionMetaPayload {
    id: String,
    cwd: String,
}

#[derive(Debug, Deserialize)]
struct TurnContextPayload {
    model: Option<String>,
    effort: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenCountInfo {
    last_token_usage: TokenUsage,
    model_context_window: u64,
}

#[derive(Debug, Deserialize)]
struct TokenCountPayload {
    info: TokenCountInfo,
}

#[derive(Debug, Deserialize)]
struct CodexLogEventEnvelope {
    #[serde(rename = "type")]
    kind: String,
    payload: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct CodexEventMessagePayload {
    #[serde(rename = "type")]
    kind: Option<String>,
    info: Option<TokenCountInfo>,
}

#[derive(Debug, Deserialize)]
struct TokenUsage {
    total_tokens: u64,
}

enum ParsedCodexSessionEvent {
    SessionMeta(SessionMetaPayload),
    TurnContext(TurnContextPayload),
    TokenCount(TokenCountInfo),
}

#[derive(Clone, Debug, Default)]
pub(super) struct FileSessionState {
    pub(super) session_id: Option<String>,
    pub(super) workspace_root: Option<String>,
    pub(super) started_unix: Option<u64>,
    pub(super) model: Option<String>,
    pub(super) effort: Option<String>,
    pub(super) context_left_percent: Option<f64>,
    pub(super) last_activity_unix: Option<u64>,
    pub(super) last_event_name: Option<String>,
    pub(super) event_count: u64,
    pub(super) tool_calls: u64,
    pub(super) tool_errors: u64,
}

#[derive(Clone, Debug)]
pub(super) struct SessionFileTracker {
    pub(super) path: PathBuf,
    pub(super) offset: u64,
    pub(super) modified_unix: u64,
    pub(super) state: FileSessionState,
}

pub(super) fn resolve_workspace_context() -> Result<WorkspaceContext> {
    let cwd = env::current_dir().context("read current working directory")?;
    let workspace_root = resolve_workspace_root(&cwd)?;
    let workspace_label = workspace_root
        .file_name()
        .and_then(|name| name.to_str())
        .map(sanitize_session_label)
        .filter(|label| !label.is_empty())
        .unwrap_or_else(|| DEFAULT_WORKSPACE_LABEL.to_string());
    let workspace_hash = workspace_hash(&workspace_root);
    let session_name = format!(
        "{SESSION_PREFIX}-{}-{}",
        workspace_label,
        &workspace_hash[..SESSION_HASH_LEN]
    );
    let metadata_path = state_home()?
        .join(STATE_DIR_RELATIVE)
        .join(format!("{workspace_hash}.json"));

    Ok(WorkspaceContext {
        workspace_root,
        workspace_label,
        workspace_hash,
        session_name,
        metadata_path,
    })
}

fn resolve_workspace_root(cwd: &Path) -> Result<PathBuf> {
    if let Ok(top_level) = git_capture(cwd, &["rev-parse", "--show-toplevel"]) {
        return fs::canonicalize(top_level.trim())
            .with_context(|| format!("canonicalize git workspace root `{}`", top_level.trim()));
    }
    fs::canonicalize(cwd).with_context(|| format!("canonicalize `{}`", cwd.display()))
}

pub(super) fn workspace_hash(root: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(root.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

pub(super) fn state_home() -> Result<PathBuf> {
    resolve_state_home(env_path("XDG_STATE_HOME"), env_path("HOME"))
}

pub(super) fn resolve_state_home(
    xdg_state_home: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(path) = xdg_state_home {
        return Ok(path);
    }
    let home = home
        .ok_or_else(|| anyhow!("cannot resolve state directory: set `XDG_STATE_HOME` or `HOME`"))?;
    Ok(home.join(".local/state"))
}

pub(super) fn env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

pub(super) fn persist_session_record(
    ctx: &WorkspaceContext,
    launcher: &str,
    launcher_args: &[String],
) -> Result<()> {
    if let Some(parent) = ctx.metadata_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create session state directory {}", parent.display()))?;
    }

    let created_at_unix = load_session_record(&ctx.metadata_path)
        .map(|record| record.created_at_unix)
        .unwrap_or_else(current_unix_seconds);

    let record = SessionRecord {
        session_name: ctx.session_name.clone(),
        workspace_root: ctx.workspace_root.display().to_string(),
        workspace_label: ctx.workspace_label.clone(),
        workspace_hash: ctx.workspace_hash.clone(),
        created_at_unix,
        launcher: launcher.to_string(),
        launcher_args: launcher_args.to_vec(),
    };

    fs::write(
        &ctx.metadata_path,
        serde_json::to_vec_pretty(&record).context("serialize codex session metadata")?,
    )
    .with_context(|| format!("write session metadata {}", ctx.metadata_path.display()))?;
    Ok(())
}

fn load_session_record(path: &Path) -> Option<SessionRecord> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub(super) fn remove_session_record(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("remove session metadata {}", path.display())),
    }
}

pub(super) fn collect_session_rows(
    tmux_sessions: &BTreeMap<String, TmuxSessionInfo>,
    tmux_available: bool,
    current_session_name: Option<&str>,
) -> Result<Vec<CodexSessionRow>> {
    let mut rows = Vec::new();
    let mut seen = BTreeSet::new();
    let records = load_session_records()?;
    let (codex_summaries, legacy_codex_context) = load_codex_session_enrichment(&records)?;

    for record in records {
        if !session_matches_scope(&record.session_name, current_session_name) {
            continue;
        }
        let tmux = tmux_sessions.get(&record.session_name);
        let codex = codex_summaries.get(&record.workspace_root);
        rows.push(CodexSessionRow {
            session_name: record.session_name.clone(),
            status: session_status_label(tmux.is_some(), true, tmux_available),
            attached_clients: tmux.map(|info| info.attached_clients).unwrap_or(0),
            last_activity_unix: tmux.and_then(|info| info.activity_unix),
            created_unix: tmux
                .and_then(|info| info.created_unix)
                .or(Some(record.created_at_unix)),
            codex_session_id: codex.map(|summary| summary.session_id.clone()),
            codex_model: codex.and_then(|summary| summary.model.clone()),
            codex_effort: codex.and_then(|summary| summary.effort.clone()),
            context_left_percent: codex.and_then(|summary| {
                summary
                    .context_left_percent
                    .or_else(|| legacy_codex_context.get(&summary.session_id).copied())
            }),
            workspace_root: Some(record.workspace_root.clone()),
            workspace_label: Some(record.workspace_label.clone()),
            metadata_present: true,
        });
        seen.insert(record.session_name);
    }

    for (name, tmux) in tmux_sessions {
        if !name.starts_with(SESSION_PREFIX) || seen.contains(name) {
            continue;
        }
        if !session_matches_scope(name, current_session_name) {
            continue;
        }
        rows.push(CodexSessionRow {
            session_name: name.clone(),
            status: session_status_label(true, false, tmux_available),
            attached_clients: tmux.attached_clients,
            last_activity_unix: tmux.activity_unix,
            created_unix: tmux.created_unix,
            codex_session_id: None,
            codex_model: None,
            codex_effort: None,
            context_left_percent: None,
            workspace_root: None,
            workspace_label: None,
            metadata_present: false,
        });
    }

    rows.sort_by(|a, b| {
        let a_running = a.status == "running";
        let b_running = b.status == "running";
        b_running
            .cmp(&a_running)
            .then_with(|| b.last_activity_unix.cmp(&a.last_activity_unix))
            .then_with(|| a.session_name.cmp(&b.session_name))
    });
    Ok(rows)
}

pub(super) fn session_matches_scope(
    session_name: &str,
    current_session_name: Option<&str>,
) -> bool {
    match current_session_name {
        Some(current) => session_name == current,
        None => true,
    }
}

fn load_codex_session_enrichment(
    records: &[SessionRecord],
) -> Result<(BTreeMap<String, CodexSessionSummary>, BTreeMap<String, f64>)> {
    let paths = codex_ps_scan_paths()?;
    load_codex_session_enrichment_for_paths(records, &paths)
}

pub(super) fn load_codex_session_enrichment_for_paths(
    records: &[SessionRecord],
    paths: &CodexPsScanPaths,
) -> Result<(BTreeMap<String, CodexSessionSummary>, BTreeMap<String, f64>)> {
    let mut cache = load_codex_ps_cache(&paths.cache_paths);
    let codex_summaries = load_codex_session_summaries(records, &paths.sessions_root, &mut cache)?;
    let missing_context_session_ids = codex_summaries
        .values()
        .filter(|summary| summary.context_left_percent.is_none())
        .map(|summary| summary.session_id.clone())
        .collect::<BTreeSet<_>>();
    let legacy_codex_context = load_legacy_codex_context_left_percent_by_session_id(
        &missing_context_session_ids,
        &paths.legacy_log_path,
        &mut cache,
    )?;
    let _ = persist_codex_ps_cache(&paths.cache_paths, &cache);
    Ok((codex_summaries, legacy_codex_context))
}

pub(super) fn load_session_records() -> Result<Vec<SessionRecord>> {
    let state_dir = state_home()?.join(STATE_DIR_RELATIVE);
    let entries = match fs::read_dir(&state_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("read session state directory {}", state_dir.display()));
        }
    };

    let mut records = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("read entry under {}", state_dir.display()))?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        if let Some(record) = load_session_record(&path) {
            records.push(record);
        }
    }
    Ok(records)
}

pub(super) fn session_record_metadata_path(record: &SessionRecord) -> Result<PathBuf> {
    Ok(state_home()?
        .join(STATE_DIR_RELATIVE)
        .join(format!("{}.json", record.workspace_hash)))
}

pub(super) fn load_codex_session_summaries(
    records: &[SessionRecord],
    sessions_root: &Path,
    cache: &mut CodexPsCache,
) -> Result<BTreeMap<String, CodexSessionSummary>> {
    let workspace_starts = records
        .iter()
        .map(|record| (record.workspace_root.clone(), record.created_at_unix))
        .collect::<BTreeMap<_, _>>();
    if workspace_starts.is_empty() {
        return Ok(BTreeMap::new());
    }

    if !sessions_root.exists() {
        return Ok(BTreeMap::new());
    }

    let mut best: BTreeMap<String, CodexSessionSummary> = BTreeMap::new();
    let mut seen_paths = BTreeSet::new();
    for dent in WalkBuilder::new(sessions_root)
        .standard_filters(false)
        .hidden(false)
        .build()
    {
        let dent = dent.with_context(|| format!("walk {}", sessions_root.display()))?;
        let path = dent.path();
        if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }

        let metadata = fs::metadata(path)
            .with_context(|| format!("read session metadata {}", path.display()))?;
        let cache_key = path.display().to_string();
        let modified_unix = file_modified_unix(&metadata);
        let len = metadata.len();
        seen_paths.insert(cache_key.clone());

        let summary = match cache.session_files.get(&cache_key) {
            Some(entry) if entry.len == len && entry.modified_unix == modified_unix => {
                entry.summary.clone()
            }
            _ => {
                let summary = scan_codex_session_file(path, modified_unix)?;
                cache.session_files.insert(
                    cache_key.clone(),
                    CachedSessionSummaryEntry {
                        len,
                        modified_unix,
                        summary: summary.clone(),
                    },
                );
                summary
            }
        };

        let Some(mut summary) = summary else {
            continue;
        };
        let Some(started_unix) = workspace_starts.get(&summary.workspace_root).copied() else {
            continue;
        };
        summary.modified_unix = summary.modified_unix.max(started_unix);
        let workspace_root = summary.workspace_root.clone();
        match best.get(&workspace_root) {
            Some(current) if current.modified_unix >= summary.modified_unix => {}
            _ => {
                best.insert(workspace_root, summary);
            }
        }
    }

    cache
        .session_files
        .retain(|path, _| seen_paths.contains(path));
    Ok(best)
}

pub(super) fn load_legacy_codex_context_left_percent_by_session_id(
    session_ids: &BTreeSet<String>,
    log_path: &Path,
    cache: &mut CodexPsCache,
) -> Result<BTreeMap<String, f64>> {
    if session_ids.is_empty() {
        return Ok(BTreeMap::new());
    }

    let metadata = match fs::metadata(log_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(err) => {
            return Err(err).with_context(|| format!("read legacy log {}", log_path.display()));
        }
    };
    let modified_unix = file_modified_unix(&metadata);
    let len = metadata.len();
    if let Some(entry) = cache
        .legacy_log
        .as_ref()
        .filter(|entry| entry.len == len && entry.modified_unix == modified_unix)
    {
        return Ok(filter_context_left_percent_values(
            &entry.values,
            session_ids,
        ));
    }

    let file = fs::File::open(log_path).with_context(|| format!("open {}", log_path.display()))?;
    let values =
        parse_legacy_codex_context_left_percent_lines_filtered(BufReader::new(file), None)?;
    let filtered = filter_context_left_percent_values(&values, session_ids);
    cache.legacy_log = Some(LegacyContextCache {
        len,
        modified_unix,
        values,
    });
    Ok(filtered)
}

fn codex_ps_scan_paths() -> Result<CodexPsScanPaths> {
    let codex_root = codex_home()?;
    Ok(CodexPsScanPaths {
        sessions_root: codex_root.join("sessions"),
        legacy_log_path: codex_root.join("log/codex-tui.log"),
        cache_paths: codex_ps_cache_paths(),
    })
}

fn codex_ps_cache_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(state_home) = state_home() {
        paths.push(state_home.join(CODEX_PS_CACHE_RELATIVE));
    }
    let temp_cache = env::temp_dir().join(CODEX_PS_CACHE_RELATIVE);
    if !paths.iter().any(|path| path == &temp_cache) {
        paths.push(temp_cache);
    }
    paths
}

fn load_codex_ps_cache(paths: &[PathBuf]) -> CodexPsCache {
    for path in paths {
        let Ok(bytes) = fs::read(path) else {
            continue;
        };
        let Ok(cache) = serde_json::from_slice::<CodexPsCache>(&bytes) else {
            continue;
        };
        if cache.version == CODEX_PS_CACHE_VERSION {
            return cache;
        }
    }
    CodexPsCache::default()
}

fn persist_codex_ps_cache(paths: &[PathBuf], cache: &CodexPsCache) -> Result<()> {
    let mut cache = cache.clone();
    cache.version = CODEX_PS_CACHE_VERSION;
    let bytes = serde_json::to_vec_pretty(&cache).context("serialize Codex ps cache")?;
    let mut last_err = None;
    for path in paths {
        if let Some(parent) = path.parent()
            && let Err(err) = fs::create_dir_all(parent)
        {
            last_err = Some(anyhow!(
                "create Codex ps cache directory {}: {}",
                parent.display(),
                err
            ));
            continue;
        }
        match fs::write(path, &bytes) {
            Ok(()) => return Ok(()),
            Err(err) => {
                last_err = Some(anyhow!("write Codex ps cache {}: {}", path.display(), err));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("no Codex ps cache path candidates available")))
}

fn file_modified_unix(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn filter_context_left_percent_values(
    values: &BTreeMap<String, f64>,
    session_ids: &BTreeSet<String>,
) -> BTreeMap<String, f64> {
    values
        .iter()
        .filter(|(session_id, _)| session_ids.contains(*session_id))
        .map(|(session_id, pct)| (session_id.clone(), *pct))
        .collect()
}

pub(super) fn codex_home() -> Result<PathBuf> {
    if let Some(path) = env_path("CODEX_HOME") {
        return Ok(path);
    }
    let home = env_path("HOME")
        .ok_or_else(|| anyhow!("cannot resolve Codex home: set `CODEX_HOME` or `HOME`"))?;
    Ok(home.join(".codex"))
}

pub(super) fn discover_codex_session_paths() -> Result<Vec<PathBuf>> {
    let sessions_root = codex_home()?.join("sessions");
    if !sessions_root.exists() {
        return Ok(Vec::new());
    }

    let mut paths = Vec::new();
    for dent in WalkBuilder::new(&sessions_root)
        .standard_filters(false)
        .hidden(false)
        .build()
    {
        let dent = dent.with_context(|| format!("walk {}", sessions_root.display()))?;
        let path = dent.path();
        if path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            paths.push(path.to_path_buf());
        }
    }
    paths.sort();
    Ok(paths)
}

impl SessionFileTracker {
    pub(super) fn new(path: PathBuf) -> Self {
        Self {
            path,
            offset: 0,
            modified_unix: 0,
            state: FileSessionState::default(),
        }
    }

    pub(super) fn sync(&mut self) -> Result<()> {
        let metadata = fs::metadata(&self.path)
            .with_context(|| format!("read session file metadata {}", self.path.display()))?;
        let len = metadata.len();
        let modified_unix = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs())
            .unwrap_or_default();

        if len < self.offset {
            self.offset = 0;
            self.state = FileSessionState::default();
        }
        if len == self.offset {
            self.modified_unix = modified_unix;
            if self.state.last_activity_unix.is_none() && modified_unix != 0 {
                self.state.last_activity_unix = Some(modified_unix);
            }
            return Ok(());
        }

        let mut file = fs::File::open(&self.path)
            .with_context(|| format!("open session file {}", self.path.display()))?;
        file.seek(SeekFrom::Start(self.offset))
            .with_context(|| format!("seek session file {}", self.path.display()))?;
        let mut reader = BufReader::new(file);
        let mut next_offset = self.offset;
        loop {
            let mut line = String::new();
            let bytes = reader
                .read_line(&mut line)
                .with_context(|| format!("read session file {}", self.path.display()))?;
            if bytes == 0 {
                break;
            }
            next_offset += bytes as u64;
            apply_session_log_line(&mut self.state, modified_unix, line.trim_end())?;
        }
        self.offset = next_offset;
        self.modified_unix = modified_unix;
        if self.state.last_activity_unix.is_none() && modified_unix != 0 {
            self.state.last_activity_unix = Some(modified_unix);
        }
        Ok(())
    }

    pub(super) fn key(&self) -> String {
        self.state
            .session_id
            .clone()
            .unwrap_or_else(|| format!("file:{}", self.path.display()))
    }
}

pub(super) fn apply_session_log_line(
    state: &mut FileSessionState,
    modified_unix: u64,
    line: &str,
) -> Result<()> {
    let value = match serde_json::from_str::<serde_json::Value>(line) {
        Ok(value) => value,
        Err(_) => return Ok(()),
    };
    let event_unix = parse_session_timestamp_unix(
        value.get("timestamp").and_then(serde_json::Value::as_str),
        modified_unix,
    );
    let kind = value
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let payload = value
        .get("payload")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    let mut event_name = None;
    match kind {
        "session_meta" => {
            if let Ok(payload) = serde_json::from_value::<SessionMetaPayload>(payload.clone()) {
                let cwd = payload.cwd.trim();
                if !cwd.is_empty() {
                    state.workspace_root = Some(cwd.to_string());
                }
                let id = payload.id.trim();
                if !id.is_empty() {
                    state.session_id = Some(id.to_string());
                }
            }
            state.started_unix = Some(state.started_unix.unwrap_or(event_unix).min(event_unix));
            event_name = Some("session_meta".to_string());
        }
        "turn_context" => {
            if let Ok(payload) = serde_json::from_value::<TurnContextPayload>(payload.clone()) {
                if let Some(model) = payload.model.filter(|value| !value.trim().is_empty()) {
                    state.model = Some(model.trim().to_string());
                }
                if let Some(effort) = payload.effort.filter(|value| !value.trim().is_empty()) {
                    state.effort = Some(effort.trim().to_string());
                }
            }
            if let Some(cwd) = payload
                .get("cwd")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                state.workspace_root = Some(cwd.to_string());
            }
            state.started_unix = Some(state.started_unix.unwrap_or(event_unix).min(event_unix));
            event_name = Some("turn_context".to_string());
        }
        "event_msg" => {
            let payload_kind = payload
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("event_msg");
            if payload_kind == "token_count"
                && let Ok(payload) = serde_json::from_value::<TokenCountPayload>(payload.clone())
            {
                state.context_left_percent = calculate_context_left_percent(
                    payload.info.last_token_usage.total_tokens,
                    payload.info.model_context_window,
                );
            }
            event_name = Some(payload_kind.to_string());
        }
        "response_item" => {
            let payload_kind = payload
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("response_item");
            match payload_kind {
                "custom_tool_call" | "function_call" => {
                    state.tool_calls = state.tool_calls.saturating_add(1);
                    if payload.get("status").and_then(serde_json::Value::as_str) == Some("failed") {
                        state.tool_errors = state.tool_errors.saturating_add(1);
                    }
                }
                "custom_tool_call_output" => {
                    if custom_tool_output_failed(&payload) {
                        state.tool_errors = state.tool_errors.saturating_add(1);
                    }
                }
                _ => {}
            }
            event_name = Some(payload_kind.to_string());
        }
        other if !other.is_empty() => {
            event_name = Some(other.to_string());
        }
        _ => {}
    }

    if let Some(event_name) = event_name {
        state.event_count = state.event_count.saturating_add(1);
        state.last_event_name = Some(event_name);
        state.last_activity_unix =
            Some(state.last_activity_unix.unwrap_or_default().max(event_unix));
    }
    Ok(())
}

fn custom_tool_output_failed(payload: &serde_json::Value) -> bool {
    let Some(output) = payload.get("output").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(output) else {
        return false;
    };
    value
        .get("metadata")
        .and_then(|metadata| metadata.get("exit_code"))
        .and_then(serde_json::Value::as_i64)
        .is_some_and(|code| code != 0)
}

fn parse_session_timestamp_unix(value: Option<&str>, fallback_unix: u64) -> u64 {
    value
        .and_then(|value| humantime::parse_rfc3339_weak(value).ok())
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(fallback_unix)
}

fn parse_codex_session_event(line: &str) -> Option<ParsedCodexSessionEvent> {
    let envelope = serde_json::from_str::<CodexLogEventEnvelope>(line).ok()?;
    match envelope.kind.as_str() {
        "session_meta" => serde_json::from_value::<SessionMetaPayload>(envelope.payload)
            .ok()
            .map(ParsedCodexSessionEvent::SessionMeta),
        "turn_context" => serde_json::from_value::<TurnContextPayload>(envelope.payload)
            .ok()
            .map(ParsedCodexSessionEvent::TurnContext),
        "event_msg" => serde_json::from_value::<CodexEventMessagePayload>(envelope.payload)
            .ok()
            .and_then(|payload| {
                (payload.kind.as_deref() == Some("token_count"))
                    .then_some(payload.info)
                    .flatten()
            })
            .map(ParsedCodexSessionEvent::TokenCount),
        _ => None,
    }
}

fn scan_codex_session_file(path: &Path, modified_unix: u64) -> Result<Option<CodexSessionSummary>> {
    let file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    scan_codex_session_lines(BufReader::new(file), modified_unix)
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn summarize_codex_session_lines<R: BufRead>(
    reader: R,
    modified_unix: u64,
    workspace_starts: &BTreeMap<String, u64>,
) -> Result<Option<CodexSessionSummary>> {
    let Some(mut summary) = scan_codex_session_lines(reader, modified_unix)? else {
        return Ok(None);
    };
    let Some(started_unix) = workspace_starts.get(&summary.workspace_root).copied() else {
        return Ok(None);
    };
    summary.modified_unix = summary.modified_unix.max(started_unix);
    Ok(Some(summary))
}

fn scan_codex_session_lines<R: BufRead>(
    reader: R,
    modified_unix: u64,
) -> Result<Option<CodexSessionSummary>> {
    let mut session_id = None;
    let mut workspace_root = None;
    let mut model = None;
    let mut effort = None;
    let mut context_left_percent = None;

    for line in reader.lines() {
        let line = line.context("read Codex session line")?;
        match parse_codex_session_event(&line) {
            Some(ParsedCodexSessionEvent::SessionMeta(payload)) => {
                session_id = Some(payload.id);
                workspace_root = Some(payload.cwd);
            }
            Some(ParsedCodexSessionEvent::TurnContext(payload)) => {
                if let Some(value) = payload.model.filter(|value| !value.is_empty()) {
                    model = Some(value);
                }
                if let Some(value) = payload.effort.filter(|value| !value.is_empty()) {
                    effort = Some(value);
                }
            }
            Some(ParsedCodexSessionEvent::TokenCount(info)) => {
                context_left_percent = calculate_context_left_percent(
                    info.last_token_usage.total_tokens,
                    info.model_context_window,
                );
            }
            None => {}
        }
    }

    let Some(session_id) = session_id else {
        return Ok(None);
    };
    let Some(workspace_root) = workspace_root else {
        return Ok(None);
    };

    Ok(Some(CodexSessionSummary {
        session_id,
        workspace_root,
        modified_unix,
        model,
        effort,
        context_left_percent,
    }))
}

pub(super) fn calculate_context_left_percent(used_tokens: u64, context_window: u64) -> Option<f64> {
    if context_window == 0 || used_tokens > context_window {
        return None;
    }
    Some(((context_window - used_tokens) as f64 / context_window as f64) * 100.0)
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn parse_legacy_codex_context_left_percent_lines<R: BufRead>(
    reader: R,
    session_ids: &BTreeSet<String>,
) -> Result<BTreeMap<String, f64>> {
    parse_legacy_codex_context_left_percent_lines_filtered(reader, Some(session_ids))
}

fn parse_legacy_codex_context_left_percent_lines_filtered<R: BufRead>(
    reader: R,
    session_ids: Option<&BTreeSet<String>>,
) -> Result<BTreeMap<String, f64>> {
    let regex = Regex::new(
        r"thread_id=(?P<session>[0-9a-f-]+).*?total_usage_tokens=(?P<used>\d+).*?auto_compact_limit=(?P<limit>\d+)",
    )
    .context("compile Codex legacy context regex")?;
    let mut usage = BTreeMap::new();

    for line in reader.lines() {
        let line = line.context("read Codex legacy context line")?;
        let Some(captures) = regex.captures(&line) else {
            continue;
        };
        let Some(session_id) = captures.name("session").map(|value| value.as_str()) else {
            continue;
        };
        if session_ids.is_some_and(|ids| !ids.contains(session_id)) {
            continue;
        }
        let Some(used_tokens) = captures
            .name("used")
            .and_then(|value| value.as_str().parse::<u64>().ok())
        else {
            continue;
        };
        let Some(auto_compact_limit) = captures
            .name("limit")
            .and_then(|value| value.as_str().parse::<u64>().ok())
        else {
            continue;
        };
        if let Some(pct) = calculate_context_left_percent(used_tokens, auto_compact_limit) {
            usage.insert(session_id.to_string(), pct);
        }
    }

    Ok(usage)
}

pub(super) fn sanitize_session_label(raw: &str) -> String {
    let mut label = String::new();
    let mut last_was_dash = false;
    for ch in raw.chars() {
        let mapped = if ch.is_ascii_alphanumeric() {
            ch.to_ascii_lowercase()
        } else {
            '-'
        };
        if mapped == '-' {
            if last_was_dash || label.is_empty() {
                continue;
            }
            last_was_dash = true;
            label.push(mapped);
            continue;
        }
        last_was_dash = false;
        label.push(mapped);
        if label.len() >= SESSION_LABEL_MAX_LEN {
            break;
        }
    }
    while label.ends_with('-') {
        label.pop();
    }
    if label.is_empty() {
        DEFAULT_WORKSPACE_LABEL.to_string()
    } else {
        label
    }
}
