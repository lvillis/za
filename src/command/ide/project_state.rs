use super::{
    ConfidenceLevel, IdeProjectRow, IdeSession, OpenedProjectsIndex, OpenedProjectsState,
    PROJECT_DISCONNECTED_GRACE_SECS, PROJECT_OPEN_SIGNAL_WORKSPACE_WINDOW_SECS,
    PROJECT_REMOTE_LIVE_MAX_AGE_SECS, PROJECT_SNAPSHOT_MAX_AGE_SECS, RecentProjectEntry,
    RemoteDevRecentSnapshot, RemoteProjectState, RemoteSessionState, RemoteSessionStateBuilder,
    toolbox_status,
};
use regex::Regex;
use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

fn remote_dev_recent_dir() -> Option<PathBuf> {
    let base = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    Some(base.join("JetBrains/RemoteDev/recent"))
}

pub(super) fn load_remote_session_state_by_pid() -> HashMap<i32, RemoteSessionState> {
    let mut state_by_pid = load_recent_remote_session_state_by_pid();
    toolbox_status::merge_toolbox_status_state(&mut state_by_pid);
    state_by_pid
}

fn load_recent_remote_session_state_by_pid() -> HashMap<i32, RemoteSessionState> {
    let Some(recent_dir) = remote_dev_recent_dir() else {
        return HashMap::new();
    };
    let entries = match fs::read_dir(recent_dir) {
        Ok(entries) => entries,
        Err(_) => return HashMap::new(),
    };
    let mut grouped: HashMap<i32, RemoteSessionStateBuilder> = HashMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(_) => continue,
        };
        let snapshot = match serde_json::from_str::<RemoteDevRecentSnapshot>(&raw) {
            Ok(snapshot) => snapshot,
            Err(_) => continue,
        };
        let freshness = file_modified_millis(&path).unwrap_or_default();
        let ide_identity_string = normalize_ide_identity(snapshot.ide_identity_string.as_deref());
        let state = grouped.entry(snapshot.app_pid).or_default();
        if freshness >= state.freshest_snapshot_millis {
            state.backend_unresponsive = snapshot.backend_unresponsive;
            state.modal_dialog_is_opened = snapshot.modal_dialog_is_opened;
            state.ide_identity_string = ide_identity_string;
            state.freshest_snapshot_millis = freshness;
        }
        for project in snapshot.projects {
            let Some(project_path) = project_path_from_recent(project.project_path.as_deref())
            else {
                continue;
            };
            let incoming = RemoteProjectState {
                project_path: project_path.clone(),
                connected: project.controller_connected,
                seconds_since_last_controller_activity: project
                    .seconds_since_last_controller_activity,
                date_last_opened_ms: project.date_last_opened,
                background_tasks_running: project.background_tasks_running,
                users: normalize_users(project.users),
                snapshot_millis: freshness,
            };
            match state.projects_by_path.get_mut(&project_path) {
                Some(existing) if incoming.snapshot_millis >= existing.snapshot_millis => {
                    *existing = incoming;
                }
                Some(_) => {}
                None => {
                    state.projects_by_path.insert(project_path, incoming);
                }
            }
        }
    }
    let mut out = HashMap::new();
    for (pid, state) in grouped {
        let mut projects = state.projects_by_path.into_values().collect::<Vec<_>>();
        projects.sort_by(|a, b| a.project_path.cmp(&b.project_path));
        out.insert(
            pid,
            RemoteSessionState {
                backend_unresponsive: state.backend_unresponsive,
                modal_dialog_is_opened: state.modal_dialog_is_opened,
                ide_identity_string: state.ide_identity_string,
                freshest_snapshot_millis: state.freshest_snapshot_millis,
                projects,
            },
        );
    }
    out
}

fn normalize_ide_identity(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    Some(normalize_project_path(value))
}

fn file_modified_millis(path: &Path) -> Option<u128> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    modified
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis())
}

fn project_path_from_recent(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    Some(normalize_project_path(value))
}

fn normalize_users(incoming: Vec<String>) -> Vec<String> {
    let mut users = Vec::new();
    for user in incoming {
        let trimmed = user.trim();
        if trimmed.is_empty() {
            continue;
        }
        if users.iter().any(|u| u == trimmed) {
            continue;
        }
        users.push(trimmed.to_string());
    }
    users.sort();
    users
}

pub(super) fn derive_project_identity(
    project_arg: &str,
    remote_projects: &[RemoteProjectState],
) -> (String, String) {
    let trimmed = project_arg.trim();
    if !trimmed.is_empty() {
        let real = normalize_project_path(trimmed);
        return (trimmed.to_string(), real);
    }
    match remote_projects {
        [single] => (single.project_path.clone(), single.project_path.clone()),
        [] => ("<unknown>".to_string(), "<unknown>".to_string()),
        _ => ("<multi-project>".to_string(), "<multi-project>".to_string()),
    }
}

pub(super) fn build_project_rows(
    sessions: &[IdeSession],
    opened_projects: &OpenedProjectsIndex,
) -> Vec<IdeProjectRow> {
    let now_ms = current_unix_millis();
    let mut rows = Vec::new();
    for session in sessions {
        let opened_state =
            opened_projects_for_session(opened_projects, session.remote_ide_identity.as_deref());
        let tracked_paths = tracked_project_paths(opened_state);
        let recent_projects = recent_remote_projects(session, now_ms, tracked_paths.as_ref());
        if recent_projects.is_empty() {
            let fallback_paths = fallback_project_paths(session, opened_state);
            for path in fallback_paths {
                rows.push(build_non_remote_project_row(session, path));
            }
            continue;
        }
        let mut seen = HashSet::new();
        for project in recent_projects {
            let project_snapshot_age_secs = snapshot_age_secs(now_ms, project.snapshot_millis);
            seen.insert(project.project_path.clone());
            rows.push(IdeProjectRow {
                pid: session.pid,
                ide: session.ide.clone(),
                ide_version: session.ide_version.clone(),
                ide_build_number: session.ide_build_number.clone(),
                project_path: project.project_path.clone(),
                controller_connected: project.connected,
                seconds_since_last_controller_activity: project
                    .seconds_since_last_controller_activity,
                date_last_opened_ms: project.date_last_opened_ms,
                project_opened_age_secs: project_opened_age_secs(
                    now_ms,
                    project.date_last_opened_ms,
                ),
                backend_unresponsive: session.remote_backend_unresponsive,
                modal_dialog_is_opened: session.remote_modal_dialog_is_opened,
                background_tasks_running: project.background_tasks_running,
                health: project_health_label(
                    session.remote_backend_unresponsive,
                    session.remote_modal_dialog_is_opened,
                    project.background_tasks_running,
                ),
                users: project.users.clone(),
                users_count: project.users.len(),
                cpu_percent: session.cpu_percent,
                rss_bytes: session.rss_bytes,
                uptime_secs: session.uptime_secs,
                child_count: session.child_count,
                shell_children: session.shell_children,
                remote_snapshot_age_secs: project_snapshot_age_secs,
                ide_station_socket_live: session.ide_station_socket_live,
                confidence: infer_confidence(
                    session.ide_station_socket_live,
                    project_snapshot_age_secs,
                ),
                duplicate_group_size: session.duplicate_group_size,
                over_limit: session.over_limit,
                orphan_due: session.orphan_due,
            });
        }
        if let Some(state) = opened_state {
            let mut missing = state
                .hot
                .iter()
                .filter(|path| !seen.contains(*path))
                .cloned()
                .collect::<Vec<_>>();
            missing.sort();
            for path in missing {
                rows.push(build_non_remote_project_row(session, path));
            }
        }
    }
    rows.sort_by(|a, b| {
        a.project_path
            .cmp(&b.project_path)
            .then_with(|| confidence_rank(b.confidence).cmp(&confidence_rank(a.confidence)))
            .then_with(|| a.ide.cmp(&b.ide))
            .then_with(|| a.pid.cmp(&b.pid))
    });
    rows
}

fn build_non_remote_project_row(session: &IdeSession, project_path: String) -> IdeProjectRow {
    IdeProjectRow {
        pid: session.pid,
        ide: session.ide.clone(),
        ide_version: session.ide_version.clone(),
        ide_build_number: session.ide_build_number.clone(),
        project_path,
        controller_connected: false,
        seconds_since_last_controller_activity: None,
        date_last_opened_ms: None,
        project_opened_age_secs: None,
        backend_unresponsive: session.remote_backend_unresponsive,
        modal_dialog_is_opened: session.remote_modal_dialog_is_opened,
        background_tasks_running: false,
        health: project_health_label(
            session.remote_backend_unresponsive,
            session.remote_modal_dialog_is_opened,
            false,
        ),
        users: Vec::new(),
        users_count: 0,
        cpu_percent: session.cpu_percent,
        rss_bytes: session.rss_bytes,
        uptime_secs: session.uptime_secs,
        child_count: session.child_count,
        shell_children: session.shell_children,
        remote_snapshot_age_secs: session.remote_snapshot_age_secs,
        ide_station_socket_live: session.ide_station_socket_live,
        confidence: infer_confidence(
            session.ide_station_socket_live,
            session.remote_snapshot_age_secs,
        ),
        duplicate_group_size: session.duplicate_group_size,
        over_limit: session.over_limit,
        orphan_due: session.orphan_due,
    }
}

fn fallback_project_paths(
    session: &IdeSession,
    opened_state: Option<&OpenedProjectsState>,
) -> Vec<String> {
    if let Some(state) = opened_state
        && !state.hot.is_empty()
    {
        let mut out = state.hot.iter().cloned().collect::<Vec<_>>();
        out.sort();
        return out;
    }
    vec![session.project_real.clone()]
}

fn recent_remote_projects<'a>(
    session: &'a IdeSession,
    now_ms: Option<u64>,
    opened_paths: Option<&HashSet<String>>,
) -> Vec<&'a RemoteProjectState> {
    if session.remote_projects.is_empty() {
        return Vec::new();
    }
    let mut recent = session
        .remote_projects
        .iter()
        .filter(|project| {
            let Some(age) = snapshot_age_secs(now_ms, project.snapshot_millis) else {
                return false;
            };
            if age > PROJECT_SNAPSHOT_MAX_AGE_SECS {
                return false;
            }
            if project.connected {
                return true;
            }
            age <= PROJECT_DISCONNECTED_GRACE_SECS
        })
        .collect::<Vec<_>>();
    if let Some(opened) = opened_paths
        && !opened.is_empty()
    {
        recent.retain(|project| {
            opened.contains(&project.project_path)
                || is_live_remote_project(project, now_ms, PROJECT_REMOTE_LIVE_MAX_AGE_SECS)
        });
    }
    if !recent.is_empty() {
        return recent;
    }
    if opened_paths.is_some_and(|opened| !opened.is_empty()) {
        return Vec::new();
    }
    if let Some(freshest) = session
        .remote_projects
        .iter()
        .max_by_key(|project| project.snapshot_millis)
        .filter(|project| {
            snapshot_age_secs(now_ms, project.snapshot_millis)
                .is_some_and(|age| age <= PROJECT_SNAPSHOT_MAX_AGE_SECS)
        })
    {
        recent.push(freshest);
    }
    recent
}

fn is_live_remote_project(
    project: &RemoteProjectState,
    now_ms: Option<u64>,
    max_age_secs: u64,
) -> bool {
    if !project.connected {
        return false;
    }
    snapshot_age_secs(now_ms, project.snapshot_millis).is_some_and(|age| age <= max_age_secs)
}

fn opened_projects_for_session<'a>(
    index: &'a OpenedProjectsIndex,
    ide_identity: Option<&str>,
) -> Option<&'a OpenedProjectsState> {
    let identity = normalize_ide_identity(ide_identity);
    if let Some(identity) = identity.as_deref()
        && let Some(paths) = index.by_identity.get(identity)
    {
        return Some(paths);
    }
    if index.by_identity.len() == 1 {
        return index.by_identity.values().next();
    }
    None
}

fn tracked_project_paths(state: Option<&OpenedProjectsState>) -> Option<HashSet<String>> {
    let state = state?;
    let mut tracked = state.opened.clone();
    tracked.extend(state.hot.iter().cloned());
    if tracked.is_empty() {
        return None;
    }
    Some(tracked)
}

pub(super) fn jetbrains_config_dir() -> Option<PathBuf> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("JetBrains"))
}

pub(super) fn load_opened_projects_index() -> OpenedProjectsIndex {
    let Some(config_dir) = jetbrains_config_dir() else {
        return OpenedProjectsIndex::default();
    };
    let now_ms = current_unix_millis();
    let entries = match fs::read_dir(config_dir) {
        Ok(entries) => entries,
        Err(_) => return OpenedProjectsIndex::default(),
    };
    let mut by_identity = HashMap::new();
    for entry in entries.flatten() {
        let identity_dir = entry.path();
        if !identity_dir.is_dir() {
            continue;
        }
        let recent_projects = identity_dir.join("options/recentProjects.xml");
        if !recent_projects.is_file() {
            continue;
        }
        let entries = parse_recent_project_entries_from_recent_projects_xml(&recent_projects);
        let mut state = OpenedProjectsState::default();
        for entry in entries {
            if entry.opened {
                state.opened.insert(entry.path);
                continue;
            }
            if workspace_touched_recently(
                &identity_dir,
                entry.workspace_id.as_deref(),
                now_ms,
                PROJECT_OPEN_SIGNAL_WORKSPACE_WINDOW_SECS,
            ) {
                state.hot.insert(entry.path);
            }
        }
        if state.opened.is_empty() && state.hot.is_empty() {
            continue;
        }
        let identity = normalize_project_path(&identity_dir.display().to_string());
        by_identity.insert(identity, state);
    }
    OpenedProjectsIndex { by_identity }
}

fn parse_recent_project_entries_from_recent_projects_xml(path: &Path) -> Vec<RecentProjectEntry> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(_) => return Vec::new(),
    };
    let entry_regex = match Regex::new(
        r#"(?s)<entry\s+key="([^"]+)">\s*<value>\s*<RecentProjectMetaInfo([^>]*)>"#,
    ) {
        Ok(regex) => regex,
        Err(_) => return Vec::new(),
    };
    let workspace_regex = match Regex::new(r#"projectWorkspaceId="([^"]+)""#) {
        Ok(regex) => regex,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for captures in entry_regex.captures_iter(&raw) {
        let key = captures.get(1).map(|m| m.as_str()).unwrap_or_default();
        let attrs = captures.get(2).map(|m| m.as_str()).unwrap_or_default();
        if let Some(path) = normalize_recent_project_entry_key(key) {
            let workspace_id = workspace_regex
                .captures(attrs)
                .and_then(|c| c.get(1))
                .map(|m| m.as_str().trim().to_string())
                .filter(|value| !value.is_empty());
            out.push(RecentProjectEntry {
                path,
                opened: attrs.contains(r#"opened="true""#),
                workspace_id,
            });
        }
    }
    out
}

fn workspace_touched_recently(
    identity_dir: &Path,
    workspace_id: Option<&str>,
    now_ms: Option<u64>,
    window_secs: u64,
) -> bool {
    let workspace_id = match workspace_id {
        Some(value) if !value.is_empty() => value,
        _ => return false,
    };
    let workspace_file = identity_dir
        .join("workspace")
        .join(format!("{workspace_id}.xml"));
    if !workspace_file.is_file() {
        return false;
    }
    snapshot_age_secs(
        now_ms,
        file_modified_millis(&workspace_file).unwrap_or_default(),
    )
    .is_some_and(|age| age <= window_secs)
}

fn normalize_recent_project_entry_key(key: &str) -> Option<String> {
    let mut value = key.trim().to_string();
    if value.is_empty() {
        return None;
    }
    if value.contains("$USER_HOME$") {
        let home = env::var("HOME").ok()?;
        value = value.replace("$USER_HOME$", &home);
    }
    if value.contains('$') {
        return None;
    }
    if !Path::new(&value).is_absolute() {
        return None;
    }
    Some(normalize_project_path(&value))
}

pub(super) fn ide_project_row_version_label(row: &IdeProjectRow) -> String {
    if let Some(version) = &row.ide_version {
        return version.clone();
    }
    if let Some(build) = &row.ide_build_number {
        return build.clone();
    }
    "-".to_string()
}

pub(super) fn current_unix_millis() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
}

fn project_opened_age_secs(now_ms: Option<u64>, opened_ms: Option<u64>) -> Option<u64> {
    let now = now_ms?;
    let opened = opened_ms?;
    now.checked_sub(opened).map(|delta_ms| delta_ms / 1000)
}

pub(super) fn snapshot_age_secs(now_ms: Option<u64>, snapshot_millis: u128) -> Option<u64> {
    if snapshot_millis == 0 {
        return None;
    }
    let now = u128::from(now_ms?);
    let age_ms = now.saturating_sub(snapshot_millis);
    u64::try_from(age_ms / 1_000).ok()
}

fn project_health_label(
    backend_unresponsive: bool,
    modal_dialog_is_opened: bool,
    background_tasks_running: bool,
) -> String {
    let mut parts = Vec::new();
    if backend_unresponsive {
        parts.push("unresp");
    }
    if modal_dialog_is_opened {
        parts.push("modal");
    }
    if background_tasks_running {
        parts.push("busy");
    }
    if parts.is_empty() {
        return "ok".to_string();
    }
    parts.join("+")
}

fn infer_confidence(
    ide_station_socket_live: bool,
    remote_snapshot_age_secs: Option<u64>,
) -> ConfidenceLevel {
    if !ide_station_socket_live {
        return ConfidenceLevel::Low;
    }
    match remote_snapshot_age_secs {
        Some(age) if age <= 120 => ConfidenceLevel::High,
        Some(age) if age <= PROJECT_SNAPSHOT_MAX_AGE_SECS => ConfidenceLevel::Medium,
        Some(_) | None => ConfidenceLevel::Low,
    }
}

fn confidence_rank(level: ConfidenceLevel) -> u8 {
    match level {
        ConfidenceLevel::High => 3,
        ConfidenceLevel::Medium => 2,
        ConfidenceLevel::Low => 1,
    }
}

pub(super) fn confidence_label(level: ConfidenceLevel) -> &'static str {
    match level {
        ConfidenceLevel::High => "high",
        ConfidenceLevel::Medium => "med",
        ConfidenceLevel::Low => "low",
    }
}

pub(super) fn normalize_project_path(project: &str) -> String {
    let p = Path::new(project);
    if let Ok(real) = fs::canonicalize(p) {
        return real.display().to_string();
    }
    let absolute = if p.is_absolute() {
        PathBuf::from(p)
    } else {
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(p)
    };
    absolute.display().to_string()
}
