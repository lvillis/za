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
    env, fs,
    path::{Path, PathBuf},
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

#[derive(Debug, Clone, Copy)]
struct ProcessIdentity {
    pid: i32,
    start_ticks: u64,
}

#[derive(Debug, Clone, Copy)]
enum StopOutcome {
    AlreadyExited,
    Stopped { forced: bool, elapsed_ms: u128 },
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

#[derive(Debug, Clone)]
struct ToolboxMainProcess {
    pid: i32,
    ppid: i32,
    start_ticks: u64,
}

#[derive(Debug, Serialize)]
struct IdeFixFailure {
    target: String,
    error: String,
}

#[derive(Debug, Serialize)]
struct IdeFixOutput {
    dry_run: bool,
    orphan_backend_pids: Vec<i32>,
    orphan_backend_stopped: Vec<i32>,
    orphan_backend_failures: Vec<IdeFixFailure>,
    toolbox_pids: Vec<i32>,
    toolbox_stopped: Vec<i32>,
    toolbox_failures: Vec<IdeFixFailure>,
    ipc_socket_path: Option<String>,
    ipc_socket_existed: bool,
    ipc_socket_removed: bool,
    semaphore_key: Option<String>,
    semaphore_ids: Vec<i32>,
    semaphore_removed: Vec<i32>,
    semaphore_failures: Vec<IdeFixFailure>,
    notes: Vec<String>,
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
    state: String,
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
        IdeCommands::Fix {
            dry_run,
            timeout_secs,
            json,
        } => run_fix(dry_run, timeout_secs, json),
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
    let mut project_rows = project_state::build_project_rows(&sessions, &opened_projects);
    sort_project_rows_for_ps(&mut project_rows);
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
        "{:<7} {:<12} {:<12} {:>5} {:>6} {:<10} {:<12} {:>5} {:>6} {:>6}  PROJECT",
        "PID", "IDE", "VER", "CTRL", "IDLE", "STATE", "HEALTH", "SHELL", "FRESH", "CONF"
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
            "{:<7} {:<12} {:<12} {:>5} {:>6} {:<10} {:<12} {:>5} {:>6} {:>6}  {}",
            row.pid,
            truncate_end(&row.ide, 12),
            version,
            conn,
            idle,
            truncate_end(&row.state, 10),
            truncate_end(&row.health, 12),
            row.shell_children,
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
    if orphan_due > 0 {
        println!(
            "Note: orphan means the backend was reparented to pid 1 and may no longer be attachable by Gateway/Toolbox."
        );
        println!(
            "Hint: review `za ide reconcile` and apply cleanup with `za ide reconcile --apply`."
        );
    }
    Ok(0)
}

fn run_stop(pid: i32, timeout_secs: u64, json: bool) -> Result<i32> {
    let sessions = collect_ide_sessions()?;
    let session = sessions
        .iter()
        .find(|s| s.pid == pid)
        .ok_or_else(|| anyhow!("PID {pid} is not a JetBrains serverMode process"))?;

    let identity = ProcessIdentity {
        pid: session.pid,
        start_ticks: session.start_ticks,
    };
    let outcome = stop_session(identity, Duration::from_secs(timeout_secs))?;
    let (stopped, forced, elapsed_ms) = match outcome {
        StopOutcome::AlreadyExited => (false, false, 0),
        StopOutcome::Stopped { forced, elapsed_ms } => (true, forced, elapsed_ms),
    };
    if json {
        let out = IdeStopOutput {
            pid: session.pid,
            ide: session.ide.clone(),
            project_real: session.project_real.clone(),
            stopped,
            forced,
            elapsed_ms,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&out).context("serialize ide stop output")?
        );
        return Ok(0);
    }

    if !stopped {
        println!(
            "ℹ️  {} (pid {}) already exited before signal delivery",
            session.ide, session.pid
        );
    } else if forced {
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
    let start_ticks_by_pid = sessions
        .iter()
        .map(|session| (session.pid, session.start_ticks))
        .collect::<HashMap<_, _>>();
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
                let identity = ProcessIdentity {
                    pid: *pid,
                    start_ticks: start_ticks_by_pid
                        .get(pid)
                        .copied()
                        .ok_or_else(|| anyhow!("missing start ticks for pid {pid}"))?,
                };
                match stop_session(identity, Duration::from_secs(timeout_secs)) {
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
            let identity = ProcessIdentity {
                pid: *pid,
                start_ticks: start_ticks_by_pid
                    .get(pid)
                    .copied()
                    .ok_or_else(|| anyhow!("missing start ticks for pid {pid}"))?,
            };
            if let Err(err) = stop_session(identity, Duration::from_secs(timeout_secs)) {
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

fn run_fix(dry_run: bool, timeout_secs: u64, json: bool) -> Result<i32> {
    let policy = za_config::load_ide_jetbrains_policy()?;
    let mut sessions = collect_ide_sessions()?;
    annotate_group_state(
        &mut sessions,
        policy.max_per_project,
        policy.orphan_ttl_minutes,
    );

    let orphan_identities = sessions
        .iter()
        .filter(|session| session.orphan_due)
        .map(|session| {
            (
                session.pid,
                ProcessIdentity {
                    pid: session.pid,
                    start_ticks: session.start_ticks,
                },
            )
        })
        .collect::<Vec<_>>();
    let toolbox_processes = collect_toolbox_main_processes()?;

    let mut orphan_backend_stopped = Vec::new();
    let mut orphan_backend_failures = Vec::new();
    let mut toolbox_stopped = Vec::new();
    let mut toolbox_failures = Vec::new();
    let timeout = Duration::from_secs(timeout_secs);

    if !dry_run {
        for (pid, identity) in &orphan_identities {
            match stop_session(*identity, timeout) {
                Ok(_) => orphan_backend_stopped.push(*pid),
                Err(err) => orphan_backend_failures.push(IdeFixFailure {
                    target: format!("pid {pid}"),
                    error: err.to_string(),
                }),
            }
        }
        for process in &toolbox_processes {
            let identity = ProcessIdentity {
                pid: process.pid,
                start_ticks: process.start_ticks,
            };
            match stop_session(identity, timeout) {
                Ok(_) => toolbox_stopped.push(process.pid),
                Err(err) => toolbox_failures.push(IdeFixFailure {
                    target: format!("pid {}", process.pid),
                    error: err.to_string(),
                }),
            }
        }
    }

    let active_toolbox = toolbox_processes
        .iter()
        .filter(|process| {
            process_matches_identity(ProcessIdentity {
                pid: process.pid,
                start_ticks: process.start_ticks,
            })
        })
        .map(|process| process.pid)
        .collect::<Vec<_>>();

    let ipc_socket_path = toolbox_ipc_socket_path();
    let ipc_socket_existed = ipc_socket_path.as_ref().is_some_and(|path| path.exists());
    let mut ipc_socket_removed = false;
    let mut notes = Vec::new();
    if !dry_run && !active_toolbox.is_empty() {
        notes.push(format!(
            "Toolbox main processes still active after stop attempt: {:?}; skipped IPC cleanup.",
            active_toolbox
        ));
    }
    if let Some(path) = ipc_socket_path.as_ref()
        && ipc_socket_existed
        && active_toolbox.is_empty()
        && !dry_run
    {
        match fs::remove_file(path) {
            Ok(()) => ipc_socket_removed = true,
            Err(err) => notes.push(format!(
                "Failed to remove Toolbox IPC socket {}: {err}",
                path.display()
            )),
        }
    }

    let semaphore_key = discover_toolbox_ipc_key();
    let semaphore_ids = match semaphore_key.as_deref() {
        Some(key) => list_semaphore_ids_by_key(key)?,
        None => Vec::new(),
    };
    let mut semaphore_removed = Vec::new();
    let mut semaphore_failures = Vec::new();
    if semaphore_key.is_none() {
        notes.push("Could not infer Toolbox IPC semaphore key from Toolbox logs.".to_string());
    }
    if !semaphore_ids.is_empty() && active_toolbox.is_empty() {
        for semid in &semaphore_ids {
            if dry_run {
                continue;
            }
            match remove_semaphore(*semid) {
                Ok(()) => semaphore_removed.push(*semid),
                Err(err) => semaphore_failures.push(IdeFixFailure {
                    target: format!("semid {semid}"),
                    error: err.to_string(),
                }),
            }
        }
    } else if !dry_run && !semaphore_ids.is_empty() && !active_toolbox.is_empty() {
        notes.push(format!(
            "Skipped semaphore cleanup for key {} because Toolbox main process(es) still appear active.",
            semaphore_key.as_deref().unwrap_or("?")
        ));
    }

    let output = IdeFixOutput {
        dry_run,
        orphan_backend_pids: orphan_identities.iter().map(|(pid, _)| *pid).collect(),
        orphan_backend_stopped,
        orphan_backend_failures,
        toolbox_pids: toolbox_processes
            .iter()
            .map(|process| process.pid)
            .collect(),
        toolbox_stopped,
        toolbox_failures,
        ipc_socket_path: ipc_socket_path.map(|path| path.display().to_string()),
        ipc_socket_existed,
        ipc_socket_removed,
        semaphore_key,
        semaphore_ids,
        semaphore_removed,
        semaphore_failures,
        notes,
    };

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&output).context("serialize ide fix output")?
        );
    } else {
        for line in render_fix_lines(&output) {
            println!("{line}");
        }
    }

    Ok((!dry_run && ide_fix_has_failures(&output)) as i32)
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

fn stop_session(identity: ProcessIdentity, timeout: Duration) -> Result<StopOutcome> {
    if !process_matches_identity(identity) {
        return Ok(StopOutcome::AlreadyExited);
    }

    let start = Instant::now();
    if !send_signal(identity, "-TERM")
        .with_context(|| format!("send SIGTERM to pid {}", identity.pid))?
    {
        return Ok(StopOutcome::AlreadyExited);
    }
    if wait_for_exit(identity, timeout) {
        return Ok(StopOutcome::Stopped {
            forced: false,
            elapsed_ms: start.elapsed().as_millis(),
        });
    }

    let sent_kill = send_signal(identity, "-KILL")
        .with_context(|| format!("send SIGKILL to pid {}", identity.pid))?;
    if !sent_kill {
        return Ok(StopOutcome::Stopped {
            forced: false,
            elapsed_ms: start.elapsed().as_millis(),
        });
    }
    if wait_for_exit(identity, Duration::from_secs(STOP_KILL_GRACE_SECS)) {
        return Ok(StopOutcome::Stopped {
            forced: true,
            elapsed_ms: start.elapsed().as_millis(),
        });
    }

    bail!("pid {} still exists after SIGKILL", identity.pid)
}

fn send_signal(identity: ProcessIdentity, signal: &str) -> Result<bool> {
    if !process_matches_identity(identity) {
        return Ok(false);
    }

    let status = Command::new("kill")
        .arg(signal)
        .arg(identity.pid.to_string())
        .status()
        .with_context(|| format!("execute kill {signal} {}", identity.pid))?;
    if !status.success() {
        if !process_matches_identity(identity) {
            return Ok(false);
        }
        bail!("kill {signal} {} failed with status {status}", identity.pid);
    }
    Ok(true)
}

fn wait_for_exit(identity: ProcessIdentity, timeout: Duration) -> bool {
    let start = Instant::now();
    while process_matches_identity(identity) && start.elapsed() < timeout {
        thread::sleep(Duration::from_millis(STOP_POLL_MS));
    }
    !process_matches_identity(identity)
}

fn process_matches_identity(identity: ProcessIdentity) -> bool {
    read_process_start_ticks(identity.pid) == Some(identity.start_ticks)
}

fn read_process_start_ticks(pid: i32) -> Option<u64> {
    let stat_path = Path::new(PROC_ROOT).join(pid.to_string()).join("stat");
    let raw = fs::read_to_string(stat_path).ok()?;
    proc_scan::parse_proc_stat_line(&raw).map(|stat| stat.start_ticks)
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

fn sort_project_rows_for_ps(rows: &mut [IdeProjectRow]) {
    rows.sort_by(|a, b| {
        project_row_attention_rank(a)
            .cmp(&project_row_attention_rank(b))
            .then_with(|| a.project_path.cmp(&b.project_path))
            .then_with(|| a.ide.cmp(&b.ide))
            .then_with(|| a.pid.cmp(&b.pid))
    });
}

fn project_row_attention_rank(row: &IdeProjectRow) -> u8 {
    if row.orphan_due {
        return 0;
    }
    if row.over_limit {
        return 1;
    }
    if row.backend_unresponsive || row.modal_dialog_is_opened {
        return 2;
    }
    if !row.ide_station_socket_live {
        return 3;
    }
    if row.remote_snapshot_age_secs.is_none() {
        return 4;
    }
    if !row.controller_connected {
        return 5;
    }
    if row.background_tasks_running {
        return 6;
    }
    7
}

fn parse_proc_pid_dir_name(name: &str) -> Option<i32> {
    if name.chars().all(|c| c.is_ascii_digit()) {
        return name.parse::<i32>().ok();
    }
    None
}

fn collect_toolbox_main_processes() -> Result<Vec<ToolboxMainProcess>> {
    let proc_root = Path::new(PROC_ROOT);
    if !proc_root.exists() {
        bail!("`za ide` is only supported on Linux with /proc");
    }
    let mut processes = Vec::new();
    for entry in fs::read_dir(proc_root).context("read /proc for toolbox main processes")? {
        let entry = match entry {
            Ok(value) => value,
            Err(_) => continue,
        };
        let Some(pid) = parse_proc_pid_dir_name(&entry.file_name().to_string_lossy()) else {
            continue;
        };
        let proc_dir = entry.path();
        let raw = fs::read(proc_dir.join("cmdline")).unwrap_or_default();
        if raw.is_empty() {
            continue;
        }
        let args = proc_scan::parse_cmdline(&raw);
        if !args
            .iter()
            .any(|arg| arg.contains("com.jetbrains.toolbox.MainKt"))
        {
            continue;
        }
        let stat_raw = match fs::read_to_string(proc_dir.join("stat")) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let Some(stat) = proc_scan::parse_proc_stat_line(&stat_raw) else {
            continue;
        };
        processes.push(ToolboxMainProcess {
            pid,
            ppid: stat.ppid,
            start_ticks: stat.start_ticks,
        });
    }
    processes.sort_by(|a, b| a.ppid.cmp(&b.ppid).then_with(|| a.pid.cmp(&b.pid)));
    Ok(processes)
}

fn toolbox_ipc_socket_path() -> Option<PathBuf> {
    let cache_home = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    Some(cache_home.join("JetBrains/Toolbox/ipc"))
}

fn toolbox_logs_dirs() -> Vec<PathBuf> {
    let Some(data_home) = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
    else {
        return Vec::new();
    };
    let base = data_home.join("JetBrains/Toolbox/logs");
    vec![base.clone(), base.join("secondary")]
}

fn discover_toolbox_ipc_key() -> Option<String> {
    let mut newest: Option<(u128, String)> = None;
    for dir in toolbox_logs_dirs() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let modified_ms = match fs::metadata(&path)
                .ok()
                .and_then(|meta| meta.modified().ok())
                .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            {
                Some(duration) => duration.as_millis(),
                None => continue,
            };
            let raw = match fs::read_to_string(&path) {
                Ok(raw) => raw,
                Err(_) => continue,
            };
            let mut found = None;
            for line in raw.lines() {
                if let Some(key) = parse_toolbox_ipc_key_line(line) {
                    found = Some(key);
                }
            }
            let Some(key) = found else {
                continue;
            };
            match &newest {
                Some((seen_ms, _)) if *seen_ms > modified_ms => {}
                _ => newest = Some((modified_ms, key)),
            }
        }
    }
    newest.map(|(_, key)| key)
}

fn parse_toolbox_ipc_key_line(line: &str) -> Option<String> {
    let (_, rest) = line.split_once("ipc_key=")?;
    let digits = rest
        .chars()
        .take_while(|ch| *ch == '-' || ch.is_ascii_digit())
        .collect::<String>();
    if digits.is_empty() {
        return None;
    }
    let value = digits.parse::<i32>().ok()?;
    Some(format!("0x{:08x}", value as u32))
}

fn list_semaphore_ids_by_key(key_hex: &str) -> Result<Vec<i32>> {
    let output = Command::new("ipcs")
        .arg("-s")
        .output()
        .context("run `ipcs -s` for Toolbox semaphore inspection")?;
    if !output.status.success() {
        bail!("`ipcs -s` failed with status {}", output.status);
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    Ok(parse_ipcs_semaphore_rows(&raw)
        .into_iter()
        .filter_map(|(key, semid)| key.eq_ignore_ascii_case(key_hex).then_some(semid))
        .collect())
}

fn parse_ipcs_semaphore_rows(raw: &str) -> Vec<(String, i32)> {
    raw.lines()
        .filter_map(|line| {
            let line = line.trim();
            if !line.starts_with("0x") && !line.starts_with("0X") {
                return None;
            }
            let mut cols = line.split_whitespace();
            let key = cols.next()?.to_ascii_lowercase();
            let semid = cols.next()?.parse::<i32>().ok()?;
            Some((key, semid))
        })
        .collect()
}

fn remove_semaphore(semid: i32) -> Result<()> {
    let status = Command::new("ipcrm")
        .args(["-s", &semid.to_string()])
        .status()
        .with_context(|| format!("run `ipcrm -s {semid}`"))?;
    if !status.success() {
        bail!("`ipcrm -s {semid}` failed with status {status}");
    }
    Ok(())
}

fn ide_fix_has_failures(output: &IdeFixOutput) -> bool {
    !output.orphan_backend_failures.is_empty()
        || !output.toolbox_failures.is_empty()
        || !output.semaphore_failures.is_empty()
}

fn render_fix_lines(output: &IdeFixOutput) -> Vec<String> {
    let status = if output.dry_run {
        "plan"
    } else if ide_fix_has_failures(output) {
        "partial"
    } else if output.orphan_backend_pids.is_empty()
        && output.toolbox_pids.is_empty()
        && !output.ipc_socket_existed
        && output.semaphore_ids.is_empty()
    {
        "clean"
    } else {
        "fixed"
    };
    let mut lines = Vec::new();
    lines.push("za ide fix".to_string());
    lines.push(format!("status         {status}"));
    lines.push(format!(
        "orphan backends {}",
        render_fix_pid_action(
            &output.orphan_backend_pids,
            &output.orphan_backend_stopped,
            output.dry_run,
            "stop"
        )
    ));
    lines.push(format!(
        "toolbox pids   {}",
        render_fix_pid_action(
            &output.toolbox_pids,
            &output.toolbox_stopped,
            output.dry_run,
            "stop"
        )
    ));
    lines.push(format!(
        "ipc socket     {}",
        match (
            &output.ipc_socket_path,
            output.ipc_socket_existed,
            output.ipc_socket_removed,
            output.dry_run
        ) {
            (Some(path), true, true, false) => format!("removed {path}"),
            (Some(path), true, true, true) => format!("would remove {path}"),
            (Some(path), true, false, true) => format!("would remove {path}"),
            (Some(path), true, false, false) => format!("present {path}"),
            (Some(path), false, _, _) => format!("absent {path}"),
            (None, _, _, _) => "unavailable".to_string(),
        }
    ));
    lines.push(format!(
        "semaphore      {}",
        match (
            &output.semaphore_key,
            output.semaphore_ids.is_empty(),
            output.semaphore_removed.is_empty(),
            output.dry_run
        ) {
            (Some(key), false, false, false) =>
                format!("removed {key} {:?}", output.semaphore_removed),
            (Some(key), false, false, true) => {
                format!("would remove {key} {:?}", output.semaphore_ids)
            }
            (Some(key), false, true, true) =>
                format!("would remove {key} {:?}", output.semaphore_ids),
            (Some(key), false, true, false) => format!("present {key} {:?}", output.semaphore_ids),
            (Some(key), true, _, _) => format!("none for {key}"),
            (None, _, _, _) => "key unavailable".to_string(),
        }
    ));
    if !output.orphan_backend_failures.is_empty()
        || !output.toolbox_failures.is_empty()
        || !output.semaphore_failures.is_empty()
        || !output.notes.is_empty()
    {
        lines.push("notes".to_string());
        for failure in &output.orphan_backend_failures {
            lines.push(format!("  - failed {}: {}", failure.target, failure.error));
        }
        for failure in &output.toolbox_failures {
            lines.push(format!("  - failed {}: {}", failure.target, failure.error));
        }
        for failure in &output.semaphore_failures {
            lines.push(format!("  - failed {}: {}", failure.target, failure.error));
        }
        for note in &output.notes {
            lines.push(format!("  - {note}"));
        }
    }
    lines
}

fn render_fix_pid_action(targets: &[i32], stopped: &[i32], dry_run: bool, verb: &str) -> String {
    if targets.is_empty() {
        return "none".to_string();
    }
    if dry_run {
        return format!("would {verb} {targets:?}");
    }
    format!("{verb} {stopped:?} (found {targets:?})")
}

#[cfg(test)]
mod tests {
    use super::proc_scan::{
        extract_jetbrains_server_session, has_jetbrains_shell_integration,
        parse_build_number_from_build_txt, parse_cmdline, parse_proc_stat_line,
    };
    use super::{
        ConfidenceLevel, IdeFixOutput, IdeProjectRow, IdeReconcileStrategy, IdeSession,
        ProcessIdentity, parse_ipcs_semaphore_rows, parse_toolbox_ipc_key_line, pick_keep_pid,
        pick_keep_pids, process_matches_identity, project_row_attention_rank,
        read_process_start_ticks, render_fix_lines, sort_project_rows_for_ps,
    };

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

    #[test]
    fn process_identity_matches_current_process_start_ticks() {
        let pid = std::process::id() as i32;
        let start_ticks = read_process_start_ticks(pid).expect("current process start ticks");
        assert!(process_matches_identity(ProcessIdentity {
            pid,
            start_ticks
        }));
        assert!(!process_matches_identity(ProcessIdentity {
            pid,
            start_ticks: start_ticks.saturating_add(1),
        }));
    }

    #[test]
    fn project_row_attention_rank_prioritizes_orphan_due_rows() {
        let orphan = sample_project_row();
        let mut live = sample_project_row();
        live.orphan_due = false;
        assert!(project_row_attention_rank(&orphan) < project_row_attention_rank(&live));
    }

    #[test]
    fn sort_project_rows_for_ps_moves_attention_rows_first() {
        let mut rows = vec![
            {
                let mut row = sample_project_row();
                row.pid = 200;
                row.orphan_due = false;
                row.state = "live".to_string();
                row
            },
            sample_project_row(),
        ];
        sort_project_rows_for_ps(&mut rows);
        assert_eq!(rows[0].pid, 100);
        assert!(rows[0].orphan_due);
    }

    #[test]
    fn parse_toolbox_ipc_key_line_converts_signed_decimal_to_hex() {
        assert_eq!(
            parse_toolbox_ipc_key_line("13:59:02 | Semaphore | ipc_key=-855630266").as_deref(),
            Some("0xcd001e46")
        );
    }

    #[test]
    fn parse_ipcs_semaphore_rows_extracts_key_and_semid() {
        let rows = parse_ipcs_semaphore_rows(
            "------ Semaphore Arrays --------\nkey        semid      owner\n0xcd001e46 1          root\n",
        );
        assert_eq!(rows, vec![("0xcd001e46".to_string(), 1)]);
    }

    #[test]
    fn render_fix_lines_mentions_socket_and_semaphore_plan() {
        let lines = render_fix_lines(&IdeFixOutput {
            dry_run: true,
            orphan_backend_pids: vec![11],
            orphan_backend_stopped: Vec::new(),
            orphan_backend_failures: Vec::new(),
            toolbox_pids: vec![22],
            toolbox_stopped: Vec::new(),
            toolbox_failures: Vec::new(),
            ipc_socket_path: Some("/root/.cache/JetBrains/Toolbox/ipc".to_string()),
            ipc_socket_existed: true,
            ipc_socket_removed: false,
            semaphore_key: Some("0xcd001e46".to_string()),
            semaphore_ids: vec![1],
            semaphore_removed: Vec::new(),
            semaphore_failures: Vec::new(),
            notes: Vec::new(),
        });
        let rendered = lines.join("\n");
        assert!(rendered.contains("would stop [11]"));
        assert!(rendered.contains("would remove /root/.cache/JetBrains/Toolbox/ipc"));
        assert!(rendered.contains("would remove 0xcd001e46 [1]"));
    }

    fn sample_project_row() -> IdeProjectRow {
        IdeProjectRow {
            pid: 100,
            ide: "rustrover".to_string(),
            ide_version: Some("2026.1".to_string()),
            ide_build_number: Some("261.1".to_string()),
            project_path: "/opt/app/joint".to_string(),
            controller_connected: true,
            seconds_since_last_controller_activity: Some(0),
            date_last_opened_ms: Some(0),
            project_opened_age_secs: Some(0),
            state: "orphan".to_string(),
            backend_unresponsive: false,
            modal_dialog_is_opened: false,
            background_tasks_running: false,
            health: "ok".to_string(),
            users: Vec::new(),
            users_count: 0,
            cpu_percent: 0.0,
            rss_bytes: 0,
            uptime_secs: 0,
            child_count: 0,
            shell_children: 0,
            remote_snapshot_age_secs: Some(0),
            ide_station_socket_live: true,
            confidence: ConfidenceLevel::High,
            duplicate_group_size: 1,
            over_limit: false,
            orphan_due: true,
        }
    }
}
