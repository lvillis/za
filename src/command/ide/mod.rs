//! JetBrains remote IDE process management.

mod proc_scan;
mod project_state;
mod toolbox_status;

use crate::{
    cli::{IdeCommands, IdeReconcileStrategy},
    command::za_config,
};
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    process::Command,
    thread,
    time::{Duration, Instant},
};

const PROC_ROOT: &str = "/proc";
const STOP_KILL_GRACE_SECS: u64 = 2;
const STOP_POLL_MS: u64 = 200;
const DEFAULT_CLK_TCK: u64 = 100;
const DEFAULT_PAGE_SIZE: u64 = 4096;
const PROJECT_SNAPSHOT_MAX_AGE_SECS: u64 = 600;
const PROJECT_DISCONNECTED_GRACE_SECS: u64 = 120;
const PROJECT_REMOTE_LIVE_MAX_AGE_SECS: u64 = 15;
const PROJECT_OPEN_SIGNAL_WORKSPACE_WINDOW_SECS: u64 = 8;

#[derive(Debug, Clone, Serialize)]
struct IdeSession {
    pid: i32,
    ppid: i32,
    uid: u32,
    ide: String,
    ide_version: Option<String>,
    ide_build_number: Option<String>,
    executable: String,
    project: String,
    project_real: String,
    cpu_percent: f64,
    rss_bytes: u64,
    uptime_secs: u64,
    child_count: usize,
    fsnotifier_children: usize,
    shell_children: usize,
    remote_backend_unresponsive: bool,
    remote_modal_dialog_is_opened: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    remote_projects: Vec<RemoteProjectState>,
    #[serde(skip_serializing)]
    remote_ide_identity: Option<String>,
    remote_snapshot_millis: u128,
    remote_snapshot_age_secs: Option<u64>,
    ide_station_socket_live: bool,
    duplicate_group_size: usize,
    over_limit: bool,
    orphan: bool,
    orphan_due: bool,
    #[serde(skip_serializing)]
    start_ticks: u64,
}

#[derive(Debug, Clone)]
struct ProcStat {
    ppid: i32,
    utime_ticks: u64,
    stime_ticks: u64,
    start_ticks: u64,
    rss_pages: i64,
}

#[derive(Debug, Clone)]
struct ChildProc {
    comm: String,
    shell_integration: bool,
}

#[derive(Debug, Clone, Serialize)]
struct RemoteProjectState {
    project_path: String,
    connected: bool,
    seconds_since_last_controller_activity: Option<u64>,
    date_last_opened_ms: Option<u64>,
    background_tasks_running: bool,
    users: Vec<String>,
    #[serde(skip_serializing)]
    snapshot_millis: u128,
}

#[derive(Debug, Clone, Default)]
struct RemoteSessionState {
    backend_unresponsive: bool,
    modal_dialog_is_opened: bool,
    ide_identity_string: Option<String>,
    freshest_snapshot_millis: u128,
    projects: Vec<RemoteProjectState>,
}

#[derive(Debug, Default)]
struct RemoteSessionStateBuilder {
    backend_unresponsive: bool,
    modal_dialog_is_opened: bool,
    ide_identity_string: Option<String>,
    freshest_snapshot_millis: u128,
    projects_by_path: HashMap<String, RemoteProjectState>,
}

#[derive(Debug, Clone, Default)]
struct IdeVersionInfo {
    version: Option<String>,
    build_number: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProductInfo {
    #[serde(default)]
    version: Option<String>,
    #[serde(rename = "buildNumber", default)]
    build_number: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RemoteDevRecentSnapshot {
    #[serde(rename = "appPid")]
    app_pid: i32,
    #[serde(rename = "ideIdentityString", default)]
    ide_identity_string: Option<String>,
    #[serde(rename = "backendUnresponsive", default)]
    backend_unresponsive: bool,
    #[serde(rename = "modalDialogIsOpened", default)]
    modal_dialog_is_opened: bool,
    #[serde(default)]
    projects: Vec<RemoteDevRecentProject>,
}

#[derive(Debug, Deserialize)]
struct RemoteDevRecentProject {
    #[serde(rename = "projectPath", default)]
    project_path: Option<String>,
    #[serde(rename = "dateLastOpened", default)]
    date_last_opened: Option<u64>,
    #[serde(rename = "controllerConnected", default)]
    controller_connected: bool,
    #[serde(rename = "secondsSinceLastControllerActivity", default)]
    seconds_since_last_controller_activity: Option<u64>,
    #[serde(rename = "backgroundTasksRunning", default)]
    background_tasks_running: bool,
    #[serde(default)]
    users: Vec<String>,
}

#[derive(Debug, Default)]
struct OpenedProjectsIndex {
    by_identity: HashMap<String, OpenedProjectsState>,
}

#[derive(Debug, Default)]
struct OpenedProjectsState {
    opened: HashSet<String>,
    hot: HashSet<String>,
}

#[derive(Debug)]
struct RecentProjectEntry {
    path: String,
    opened: bool,
    workspace_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct IdePsOutput {
    total: usize,
    total_projects: usize,
    duplicate_groups: usize,
    max_per_project: usize,
    orphan_ttl_minutes: u64,
    orphan_due: usize,
    projects: Vec<IdeProjectRow>,
}

#[derive(Debug, Serialize)]
struct IdeStopOutput {
    pid: i32,
    ide: String,
    project_real: String,
    stopped: bool,
    forced: bool,
    elapsed_ms: u128,
}

#[derive(Debug, Serialize)]
struct ReconcileFailure {
    pid: i32,
    error: String,
}

#[derive(Debug, Serialize)]
struct ReconcileGroupOutput {
    uid: u32,
    ide: String,
    project_real: String,
    keep_pid: i32,
    keep_pids: Vec<i32>,
    max_per_project: usize,
    stop_pids: Vec<i32>,
    applied: bool,
    stopped_pids: Vec<i32>,
    failures: Vec<ReconcileFailure>,
}

#[derive(Debug, Serialize)]
struct IdeReconcileOutput {
    duplicate_groups: usize,
    planned_stops: usize,
    applied: bool,
    max_per_project: usize,
    orphan_ttl_minutes: u64,
    groups: Vec<ReconcileGroupOutput>,
    orphan_pids: Vec<i32>,
    orphan_failures: Vec<ReconcileFailure>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
enum ConfidenceLevel {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Serialize)]
struct IdeProjectRow {
    pid: i32,
    ide: String,
    ide_version: Option<String>,
    ide_build_number: Option<String>,
    project_path: String,
    controller_connected: bool,
    seconds_since_last_controller_activity: Option<u64>,
    date_last_opened_ms: Option<u64>,
    project_opened_age_secs: Option<u64>,
    backend_unresponsive: bool,
    modal_dialog_is_opened: bool,
    background_tasks_running: bool,
    health: String,
    users: Vec<String>,
    users_count: usize,
    cpu_percent: f64,
    rss_bytes: u64,
    uptime_secs: u64,
    child_count: usize,
    shell_children: usize,
    remote_snapshot_age_secs: Option<u64>,
    ide_station_socket_live: bool,
    confidence: ConfidenceLevel,
    duplicate_group_size: usize,
    over_limit: bool,
    orphan_due: bool,
}

pub fn run(cmd: IdeCommands) -> Result<i32> {
    match cmd {
        IdeCommands::Ps { duplicates, json } => run_ps(duplicates, json),
        IdeCommands::Stop {
            pid,
            timeout_secs,
            json,
        } => run_stop(pid, timeout_secs, json),
        IdeCommands::Reconcile {
            apply,
            keep,
            timeout_secs,
            json,
        } => run_reconcile(apply, keep, timeout_secs, json),
    }
}

fn run_ps(duplicates_only: bool, json: bool) -> Result<i32> {
    let policy = za_config::load_ide_jetbrains_policy()?;
    let mut sessions = collect_ide_sessions()?;
    let opened_projects = project_state::load_opened_projects_index();
    annotate_group_state(
        &mut sessions,
        policy.max_per_project,
        policy.orphan_ttl_minutes,
    );
    if duplicates_only {
        sessions.retain(|s| s.over_limit);
    }

    let duplicate_groups = over_limit_group_count(&sessions, policy.max_per_project);
    let orphan_due = sessions.iter().filter(|s| s.orphan_due).count();
    let project_rows = project_state::build_project_rows(&sessions, &opened_projects);
    if json {
        let out = IdePsOutput {
            total: sessions.len(),
            total_projects: project_rows.len(),
            duplicate_groups,
            max_per_project: policy.max_per_project,
            orphan_ttl_minutes: policy.orphan_ttl_minutes,
            orphan_due,
            projects: project_rows,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&out).context("serialize ide ps output")?
        );
        return Ok(0);
    }

    if project_rows.is_empty() {
        if duplicates_only {
            println!("No duplicate JetBrains remote IDE sessions found.");
        } else {
            println!("No JetBrains remote IDE sessions found.");
        }
        return Ok(0);
    }

    println!(
        "{:<7} {:<12} {:<12} {:>5} {:>6} {:>5} {:<12} {:>6} {:>6}  PROJECT",
        "PID", "IDE", "VER", "CONN", "IDLE", "SHELL", "HEALTH", "FRESH", "CONF"
    );
    for row in &project_rows {
        let version = truncate_end(&project_state::ide_project_row_version_label(row), 12);
        let conn = if row.controller_connected { "yes" } else { "-" };
        let idle = row
            .seconds_since_last_controller_activity
            .map(|secs| format!("{secs}s"))
            .unwrap_or_else(|| "-".to_string());
        let freshness = row
            .remote_snapshot_age_secs
            .map(|secs| format!("{secs}s"))
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{:<7} {:<12} {:<12} {:>5} {:>6} {:>5} {:<12} {:>6} {:>6}  {}",
            row.pid,
            truncate_end(&row.ide, 12),
            version,
            conn,
            idle,
            row.shell_children,
            truncate_end(&row.health, 12),
            freshness,
            project_state::confidence_label(row.confidence),
            row.project_path
        );
    }
    println!();
    println!("Total sessions: {}", sessions.len());
    println!("Total projects: {}", project_rows.len());
    println!(
        "Over-limit groups (max_per_project={}): {duplicate_groups}",
        policy.max_per_project
    );
    println!(
        "Orphan due sessions (ttl={}m): {}",
        policy.orphan_ttl_minutes, orphan_due
    );
    Ok(0)
}

fn run_stop(pid: i32, timeout_secs: u64, json: bool) -> Result<i32> {
    let sessions = collect_ide_sessions()?;
    let session = sessions
        .iter()
        .find(|s| s.pid == pid)
        .ok_or_else(|| anyhow!("PID {pid} is not a JetBrains serverMode process"))?;

    let (forced, elapsed_ms) = stop_session(pid, Duration::from_secs(timeout_secs))?;
    if json {
        let out = IdeStopOutput {
            pid: session.pid,
            ide: session.ide.clone(),
            project_real: session.project_real.clone(),
            stopped: true,
            forced,
            elapsed_ms,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&out).context("serialize ide stop output")?
        );
        return Ok(0);
    }

    if forced {
        println!(
            "🛑 Stopped {} (pid {}) with SIGKILL after timeout ({} ms)",
            session.ide, session.pid, elapsed_ms
        );
    } else {
        println!(
            "✅ Stopped {} (pid {}) with SIGTERM ({} ms)",
            session.ide, session.pid, elapsed_ms
        );
    }
    Ok(0)
}

fn run_reconcile(
    apply: bool,
    keep: IdeReconcileStrategy,
    timeout_secs: u64,
    json: bool,
) -> Result<i32> {
    let policy = za_config::load_ide_jetbrains_policy()?;
    let mut sessions = collect_ide_sessions()?;
    annotate_group_state(
        &mut sessions,
        policy.max_per_project,
        policy.orphan_ttl_minutes,
    );

    let mut groups: HashMap<String, Vec<IdeSession>> = HashMap::new();
    for session in &sessions {
        groups
            .entry(group_key(session))
            .or_default()
            .push(session.clone());
    }

    let mut keys = groups.keys().cloned().collect::<Vec<_>>();
    keys.sort();

    let mut outputs = Vec::new();
    let mut planned_stops = 0usize;
    let mut planned_group_stop_pids = HashSet::new();
    for key in keys {
        let mut grouped = groups
            .remove(&key)
            .ok_or_else(|| anyhow!("reconcile group disappeared"))?;
        if grouped.len() <= policy.max_per_project {
            continue;
        }

        grouped.sort_by_key(|s| (s.start_ticks, s.pid));
        let keep_pids = pick_keep_pids(&grouped, keep, policy.max_per_project);
        let keep_pid = pick_keep_pid(&grouped, keep);
        let keep_set = keep_pids.iter().copied().collect::<HashSet<_>>();
        let keep_session = grouped
            .iter()
            .find(|s| s.pid == keep_pid)
            .ok_or_else(|| anyhow!("keep PID {keep_pid} not found in group"))?;
        let stop_pids = grouped
            .iter()
            .filter(|s| !keep_set.contains(&s.pid))
            .map(|s| s.pid)
            .collect::<Vec<_>>();
        planned_stops += stop_pids.len();
        for pid in &stop_pids {
            planned_group_stop_pids.insert(*pid);
        }

        outputs.push(ReconcileGroupOutput {
            uid: keep_session.uid,
            ide: keep_session.ide.clone(),
            project_real: keep_session.project_real.clone(),
            keep_pid,
            keep_pids,
            max_per_project: policy.max_per_project,
            stop_pids,
            applied: apply,
            stopped_pids: Vec::new(),
            failures: Vec::new(),
        });
    }

    let orphan_pids = sessions
        .iter()
        .filter(|s| s.orphan_due && !planned_group_stop_pids.contains(&s.pid))
        .map(|s| s.pid)
        .collect::<Vec<_>>();
    planned_stops += orphan_pids.len();

    if outputs.is_empty() && orphan_pids.is_empty() {
        if json {
            let out = IdeReconcileOutput {
                duplicate_groups: 0,
                planned_stops: 0,
                applied: apply,
                max_per_project: policy.max_per_project,
                orphan_ttl_minutes: policy.orphan_ttl_minutes,
                groups: Vec::new(),
                orphan_pids: Vec::new(),
                orphan_failures: Vec::new(),
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&out).context("serialize ide reconcile output")?
            );
        } else {
            println!("No over-limit or orphan-due JetBrains IDE sessions to reconcile.");
        }
        return Ok(0);
    }

    let mut total_failures = 0usize;
    let mut orphan_failures = Vec::new();
    if apply {
        for group in &mut outputs {
            for pid in &group.stop_pids {
                match stop_session(*pid, Duration::from_secs(timeout_secs)) {
                    Ok(_) => group.stopped_pids.push(*pid),
                    Err(err) => group.failures.push(ReconcileFailure {
                        pid: *pid,
                        error: err.to_string(),
                    }),
                }
            }
            total_failures += group.failures.len();
        }
        for pid in &orphan_pids {
            if let Err(err) = stop_session(*pid, Duration::from_secs(timeout_secs)) {
                orphan_failures.push(ReconcileFailure {
                    pid: *pid,
                    error: err.to_string(),
                });
            }
        }
        total_failures += orphan_failures.len();
    }

    if json {
        let out = IdeReconcileOutput {
            duplicate_groups: outputs.len(),
            planned_stops,
            applied: apply,
            max_per_project: policy.max_per_project,
            orphan_ttl_minutes: policy.orphan_ttl_minutes,
            groups: outputs,
            orphan_pids,
            orphan_failures,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&out).context("serialize ide reconcile output")?
        );
    } else {
        if apply {
            println!("Applying duplicate session reconciliation:");
        } else {
            println!("Dry-run duplicate session reconciliation plan:");
        }
        for group in &outputs {
            println!(
                "- {} uid={} project={} keep={:?} stop={:?}",
                group.ide, group.uid, group.project_real, group.keep_pids, group.stop_pids
            );
            if !group.failures.is_empty() {
                for failure in &group.failures {
                    println!("  failed pid {}: {}", failure.pid, failure.error);
                }
            }
        }
        if !orphan_pids.is_empty() {
            println!(
                "- orphan ttl ({}m) stop={:?}",
                policy.orphan_ttl_minutes, orphan_pids
            );
            for failure in &orphan_failures {
                println!("  failed orphan pid {}: {}", failure.pid, failure.error);
            }
        }
        println!(
            "Over-limit groups: {} | Planned stops: {}{}",
            outputs.len(),
            planned_stops,
            if apply {
                format!(
                    " | Succeeded: {} | Failed: {}",
                    outputs.iter().map(|g| g.stopped_pids.len()).sum::<usize>()
                        + orphan_pids.len().saturating_sub(orphan_failures.len()),
                    total_failures
                )
            } else {
                String::new()
            }
        );
    }

    if apply && total_failures > 0 {
        bail!("reconcile completed with {total_failures} stop failures");
    }
    Ok(0)
}

fn collect_ide_sessions() -> Result<Vec<IdeSession>> {
    proc_scan::collect_ide_sessions()
}

fn annotate_group_state(
    sessions: &mut [IdeSession],
    max_per_project: usize,
    orphan_ttl_minutes: u64,
) {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for session in sessions.iter() {
        *counts.entry(group_key(session)).or_default() += 1;
    }
    for session in sessions.iter_mut() {
        let group_count = counts.get(&group_key(session)).copied().unwrap_or(1);
        session.duplicate_group_size = group_count;
        session.over_limit = group_count > max_per_project;
        let ttl_secs = orphan_ttl_minutes.saturating_mul(60);
        session.orphan_due = session.orphan && session.uptime_secs >= ttl_secs;
    }
}

fn over_limit_group_count(sessions: &[IdeSession], max_per_project: usize) -> usize {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for session in sessions {
        *counts.entry(group_key(session)).or_default() += 1;
    }
    counts.values().filter(|v| **v > max_per_project).count()
}

fn group_key(session: &IdeSession) -> String {
    format!("{}:{}:{}", session.uid, session.ide, session.project_real)
}

fn pick_keep_pid(sessions: &[IdeSession], strategy: IdeReconcileStrategy) -> i32 {
    match strategy {
        IdeReconcileStrategy::Newest => sessions
            .iter()
            .max_by_key(|s| (s.start_ticks, s.pid))
            .map(|s| s.pid)
            .unwrap_or_default(),
        IdeReconcileStrategy::Oldest => sessions
            .iter()
            .min_by_key(|s| (s.start_ticks, s.pid))
            .map(|s| s.pid)
            .unwrap_or_default(),
    }
}

fn pick_keep_pids(
    sessions: &[IdeSession],
    strategy: IdeReconcileStrategy,
    max_per_project: usize,
) -> Vec<i32> {
    if sessions.is_empty() {
        return Vec::new();
    }
    if max_per_project >= sessions.len() {
        return sessions.iter().map(|s| s.pid).collect();
    }

    let mut ordered = sessions.to_vec();
    ordered.sort_by_key(|s| (s.start_ticks, s.pid));
    let mut selected = match strategy {
        IdeReconcileStrategy::Newest => ordered
            .iter()
            .rev()
            .take(max_per_project)
            .map(|s| s.pid)
            .collect::<Vec<_>>(),
        IdeReconcileStrategy::Oldest => ordered
            .iter()
            .take(max_per_project)
            .map(|s| s.pid)
            .collect::<Vec<_>>(),
    };
    selected.sort();
    selected
}

fn stop_session(pid: i32, timeout: Duration) -> Result<(bool, u128)> {
    if !process_exists(pid) {
        return Ok((false, 0));
    }

    let start = Instant::now();
    send_signal(pid, "-TERM").with_context(|| format!("send SIGTERM to pid {pid}"))?;
    if wait_for_exit(pid, timeout) {
        return Ok((false, start.elapsed().as_millis()));
    }

    send_signal(pid, "-KILL").with_context(|| format!("send SIGKILL to pid {pid}"))?;
    if wait_for_exit(pid, Duration::from_secs(STOP_KILL_GRACE_SECS)) {
        return Ok((true, start.elapsed().as_millis()));
    }

    bail!("pid {pid} still exists after SIGKILL")
}

fn send_signal(pid: i32, signal: &str) -> Result<()> {
    let status = Command::new("kill")
        .arg(signal)
        .arg(pid.to_string())
        .status()
        .with_context(|| format!("execute kill {signal} {pid}"))?;
    if !status.success() {
        bail!("kill {signal} {pid} failed with status {status}");
    }
    Ok(())
}

fn wait_for_exit(pid: i32, timeout: Duration) -> bool {
    let start = Instant::now();
    while process_exists(pid) && start.elapsed() < timeout {
        thread::sleep(Duration::from_millis(STOP_POLL_MS));
    }
    !process_exists(pid)
}

fn process_exists(pid: i32) -> bool {
    Path::new(PROC_ROOT).join(pid.to_string()).exists()
}

fn truncate_end(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }
    let mut out = input.chars().take(max_chars - 3).collect::<String>();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::proc_scan::{
        extract_jetbrains_server_session, has_jetbrains_shell_integration,
        parse_build_number_from_build_txt, parse_cmdline, parse_proc_stat_line,
    };
    use super::{IdeReconcileStrategy, IdeSession, pick_keep_pid, pick_keep_pids};

    #[test]
    fn parse_cmdline_splits_nul_parts() {
        let raw = b"/bin/rustrover\0serverMode\0/opt/app/proj\0";
        let args = parse_cmdline(raw);
        assert_eq!(args, vec!["/bin/rustrover", "serverMode", "/opt/app/proj"]);
    }

    #[test]
    fn parse_proc_stat_extracts_fields() {
        let line = "123 (rustrover) S 1 2 3 4 5 6 7 8 9 10 120 30 13 14 15 16 17 18 5000 20 4096";
        let stat = parse_proc_stat_line(line).expect("valid stat");
        assert_eq!(stat.ppid, 1);
        assert_eq!(stat.utime_ticks, 120);
        assert_eq!(stat.stime_ticks, 30);
        assert_eq!(stat.start_ticks, 5000);
        assert_eq!(stat.rss_pages, 4096);
    }

    #[test]
    fn extract_jetbrains_server_session_detects_expected_shape() {
        let args = vec![
            "/root/.local/share/JetBrains/Toolbox/apps/rustrover/bin/rustrover".to_string(),
            "serverMode".to_string(),
            "/opt/app/s3-rs".to_string(),
        ];
        let (ide, exe, project) =
            extract_jetbrains_server_session(&args).expect("must be detected");
        assert_eq!(ide, "rustrover");
        assert!(exe.contains("JetBrains"));
        assert_eq!(project, "/opt/app/s3-rs");
    }

    #[test]
    fn extract_jetbrains_server_session_allows_missing_project_arg() {
        let args = vec![
            "/root/.local/share/JetBrains/Toolbox/apps/rustrover/bin/rustrover".to_string(),
            "serverMode".to_string(),
        ];
        let (ide, _, project) = extract_jetbrains_server_session(&args).expect("must be detected");
        assert_eq!(ide, "rustrover");
        assert_eq!(project, "");
    }

    #[test]
    fn shell_integration_detection_matches_expected_arg() {
        let args = vec![
            "/bin/bash".to_string(),
            "--rcfile".to_string(),
            "/root/.local/share/JetBrains/Toolbox/apps/rustrover/plugins/terminal/shell-integrations/bash/bash-integration.bash".to_string(),
            "-i".to_string(),
        ];
        assert!(has_jetbrains_shell_integration(&args));
    }

    #[test]
    fn shell_integration_detection_rejects_plain_shell() {
        let args = vec!["/bin/bash".to_string(), "-i".to_string()];
        assert!(!has_jetbrains_shell_integration(&args));
    }

    #[test]
    fn parse_build_number_from_build_txt_strips_product_prefix() {
        let raw = "RR-253.31033.132\n";
        assert_eq!(
            parse_build_number_from_build_txt(raw).as_deref(),
            Some("253.31033.132")
        );
    }

    #[test]
    fn pick_keep_pid_respects_strategy() {
        let sessions = vec![
            IdeSession {
                pid: 100,
                ppid: 1,
                uid: 0,
                ide: "rustrover".to_string(),
                ide_version: None,
                ide_build_number: None,
                executable: "/bin/rustrover".to_string(),
                project: "/a".to_string(),
                project_real: "/a".to_string(),
                cpu_percent: 1.0,
                rss_bytes: 1,
                uptime_secs: 1,
                child_count: 0,
                fsnotifier_children: 0,
                shell_children: 0,
                remote_backend_unresponsive: false,
                remote_modal_dialog_is_opened: false,
                remote_projects: Vec::new(),
                remote_ide_identity: None,
                remote_snapshot_millis: 0,
                remote_snapshot_age_secs: None,
                ide_station_socket_live: false,
                duplicate_group_size: 2,
                over_limit: true,
                orphan: true,
                orphan_due: false,
                start_ticks: 1000,
            },
            IdeSession {
                pid: 200,
                ppid: 1,
                uid: 0,
                ide: "rustrover".to_string(),
                ide_version: None,
                ide_build_number: None,
                executable: "/bin/rustrover".to_string(),
                project: "/a".to_string(),
                project_real: "/a".to_string(),
                cpu_percent: 1.0,
                rss_bytes: 1,
                uptime_secs: 1,
                child_count: 0,
                fsnotifier_children: 0,
                shell_children: 0,
                remote_backend_unresponsive: false,
                remote_modal_dialog_is_opened: false,
                remote_projects: Vec::new(),
                remote_ide_identity: None,
                remote_snapshot_millis: 0,
                remote_snapshot_age_secs: None,
                ide_station_socket_live: false,
                duplicate_group_size: 2,
                over_limit: true,
                orphan: true,
                orphan_due: false,
                start_ticks: 2000,
            },
        ];
        assert_eq!(pick_keep_pid(&sessions, IdeReconcileStrategy::Newest), 200);
        assert_eq!(pick_keep_pid(&sessions, IdeReconcileStrategy::Oldest), 100);
        assert_eq!(
            pick_keep_pids(&sessions, IdeReconcileStrategy::Newest, 1),
            vec![200]
        );
        assert_eq!(
            pick_keep_pids(&sessions, IdeReconcileStrategy::Oldest, 1),
            vec![100]
        );
    }
}
