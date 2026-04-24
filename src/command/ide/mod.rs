//! JetBrains remote IDE process management.

mod proc_scan;
mod project_state;
mod toolbox_status;

use crate::{
    cli::{IdeAgentCommands, IdeCommands, IdeReconcileStrategy},
    command::{paths, za_config},
};
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
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
const IDE_AGENT_BASH_START_MARKER: &str = "# >>> za ide agent shims (bash) >>>";
const IDE_AGENT_BASH_END_MARKER: &str = "# <<< za ide agent shims (bash) <<<";
const IDE_AGENT_SHIM_MANAGED_MARKER: &str = "# za-managed: ide-agent-shim v1";

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

#[derive(Debug, Serialize)]
struct IdeAgentStatusOutput {
    shim_dir: String,
    bashrc_path: String,
    bashrc_configured: bool,
    agents: Vec<IdeAgentStatus>,
}

#[derive(Debug, Serialize)]
struct IdeAgentStatus {
    agent: String,
    shim_path: String,
    shim_installed: bool,
    shim_managed: bool,
    shim_current: bool,
    run_target: Option<String>,
    probe: Option<IdeAgentProbe>,
    issues: Vec<String>,
}

#[derive(Debug, Serialize)]
struct IdeAgentProbe {
    command: String,
    success: bool,
    resolved: Option<String>,
    exit_code: Option<i32>,
    stderr: String,
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
        IdeCommands::Agent { cmd } => run_agent(cmd),
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

fn run_agent(cmd: IdeAgentCommands) -> Result<i32> {
    match cmd {
        IdeAgentCommands::Install { agent, force } => run_agent_install(&agent, force),
        IdeAgentCommands::Status { agent, probe, json } => {
            run_agent_status(agent.as_deref(), probe, json)
        }
        IdeAgentCommands::Uninstall { agent, force } => run_agent_uninstall(&agent, force),
    }
}

fn run_agent_install(agent: &str, force: bool) -> Result<i32> {
    let agent = normalize_ide_agent_name(agent)?;
    let run_target = crate::command::run::resolve_executable_path(&agent)
        .with_context(|| format!("resolve real `{agent}` executable for `za run {agent}`"))?;
    let za_executable = resolve_current_za_executable()?;
    let shim_dir = paths::jetbrains_agent_shim_bin_dir()?;
    let shim_path = ide_agent_shim_path(&shim_dir, &agent);
    fs::create_dir_all(&shim_dir).with_context(|| {
        format!(
            "create JetBrains agent shim directory `{}`",
            shim_dir.display()
        )
    })?;
    let expected_shim = expected_ide_agent_shim_content(&agent, &za_executable, &run_target);
    if let Some(existing) = read_optional_file_lossy(&shim_path)? {
        if existing != expected_shim && !ide_agent_shim_is_managed(&existing) && !force {
            bail!(
                "refusing to overwrite non-za-managed shim `{}`; pass `--force` to replace it",
                shim_path.display()
            );
        }
    }
    fs::write(&shim_path, expected_shim)
        .with_context(|| format!("write JetBrains agent shim `{}`", shim_path.display()))?;
    #[cfg(unix)]
    {
        fs::set_permissions(&shim_path, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("chmod +x `{}`", shim_path.display()))?;
    }

    let bashrc_path = paths::home_dir()?.join(".bashrc");
    let bashrc_removed = remove_ide_agent_bash_block(&bashrc_path)?;
    let legacy_shim_removed = remove_managed_legacy_ide_agent_shim(&agent)?;

    println!("installed JetBrains agent shim");
    println!("agent          {agent}");
    println!("shim           {}", shim_path.display());
    println!("runs           za run {agent}");
    println!("za             {}", za_executable.display());
    println!("target         {}", run_target.display());
    println!(
        "legacy cleanup {}",
        match (bashrc_removed, legacy_shim_removed) {
            (false, false) => "unchanged",
            (true, false) => "removed bashrc PATH block",
            (false, true) => "removed old shim",
            (true, true) => "removed old shim and bashrc PATH block",
        }
    );
    Ok(0)
}

fn run_agent_status(agent: Option<&str>, probe: bool, json: bool) -> Result<i32> {
    let output = collect_ide_agent_status(agent, probe)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&output).context("serialize ide agent status output")?
        );
        return Ok(0);
    }

    println!("za ide agent status");
    println!("shim dir       {}", output.shim_dir);
    println!(
        "legacy bashrc  {} ({})",
        output.bashrc_path,
        if output.bashrc_configured {
            "configured"
        } else {
            "missing"
        }
    );
    if output.agents.is_empty() {
        println!("agents         none");
        return Ok(0);
    }
    println!(
        "{:<18} {:<10} {:<10} {:<10} TARGET",
        "AGENT", "SHIM", "MANAGED", "CURRENT"
    );
    for row in output.agents {
        println!(
            "{:<18} {:<10} {:<10} {:<10} {}",
            row.agent,
            if row.shim_installed {
                "installed"
            } else {
                "missing"
            },
            if row.shim_managed { "yes" } else { "no" },
            if row.shim_current { "yes" } else { "no" },
            row.run_target.as_deref().unwrap_or("-")
        );
        if let Some(probe) = row.probe {
            println!(
                "  probe        {}",
                probe.resolved.as_deref().unwrap_or("unresolved")
            );
        }
        for issue in row.issues {
            println!("  note         {issue}");
        }
    }
    Ok(0)
}

fn run_agent_uninstall(agent: &str, force: bool) -> Result<i32> {
    let agent = normalize_ide_agent_name(agent)?;
    let shim_dir = paths::jetbrains_agent_shim_bin_dir()?;
    let shim_path = ide_agent_shim_path(&shim_dir, &agent);
    if let Some(existing) = read_optional_file_lossy(&shim_path)?
        && !ide_agent_shim_is_managed(&existing)
        && !force
    {
        bail!(
            "refusing to remove non-za-managed shim `{}`; pass `--force` to remove it",
            shim_path.display()
        );
    }
    let removed = match fs::remove_file(&shim_path) {
        Ok(()) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => return Err(err).with_context(|| format!("remove `{}`", shim_path.display())),
    };

    let legacy_removed = remove_managed_legacy_ide_agent_shim(&agent)?;
    let bashrc_path = paths::home_dir()?.join(".bashrc");
    let bashrc_removed = remove_ide_agent_bash_block(&bashrc_path)?;

    println!("removed JetBrains agent shim");
    println!("agent          {agent}");
    println!(
        "shim           {}",
        if removed { "removed" } else { "absent" }
    );
    println!(
        "legacy cleanup {}",
        match (bashrc_removed, legacy_removed) {
            (false, false) => "unchanged",
            (true, false) => "removed bashrc PATH block",
            (false, true) => "removed old shim",
            (true, true) => "removed old shim and bashrc PATH block",
        }
    );
    Ok(0)
}

fn collect_ide_agent_status(agent: Option<&str>, probe: bool) -> Result<IdeAgentStatusOutput> {
    let shim_dir = paths::jetbrains_agent_shim_bin_dir()?;
    let bashrc_path = paths::home_dir()?.join(".bashrc");
    let agents = match agent {
        Some(agent) => vec![normalize_ide_agent_name(agent)?],
        None => list_installed_ide_agent_names(&shim_dir)?,
    };
    let bashrc_configured = ide_agent_bash_block_is_configured(&bashrc_path, &shim_dir)?;
    let rows = agents
        .into_iter()
        .map(|agent| collect_single_ide_agent_status(&shim_dir, agent, probe))
        .collect::<Result<Vec<_>>>()?;
    Ok(IdeAgentStatusOutput {
        shim_dir: shim_dir.display().to_string(),
        bashrc_path: bashrc_path.display().to_string(),
        bashrc_configured,
        agents: rows,
    })
}

fn collect_single_ide_agent_status(
    shim_dir: &Path,
    agent: String,
    probe: bool,
) -> Result<IdeAgentStatus> {
    let shim_path = ide_agent_shim_path(shim_dir, &agent);
    let za_executable = resolve_current_za_executable().ok();
    let run_target = match crate::command::run::resolve_executable_path(&agent) {
        Ok(path) => Some(path),
        Err(err) => {
            let mut issues = Vec::new();
            issues.push(format!(
                "`za run {agent}` cannot resolve a real executable: {err}"
            ));
            let probe_result = if probe {
                Some(probe_jetbrains_agent_command(&agent))
            } else {
                None
            };
            return Ok(IdeAgentStatus {
                agent,
                shim_path: shim_path.display().to_string(),
                shim_installed: shim_path.exists(),
                shim_managed: read_optional_file_lossy(&shim_path)?
                    .as_deref()
                    .is_some_and(ide_agent_shim_is_managed),
                shim_current: false,
                run_target: None,
                probe: probe_result,
                issues,
            });
        }
    };
    let expected = za_executable
        .as_deref()
        .zip(run_target.as_deref())
        .map(|(za, target)| expected_ide_agent_shim_content(&agent, za, target));
    let shim_content = read_optional_file_lossy(&shim_path)?;
    let shim_installed = shim_content.is_some();
    let shim_managed = shim_content
        .as_deref()
        .is_some_and(ide_agent_shim_is_managed);
    let shim_current = expected
        .as_deref()
        .is_some_and(|expected| shim_content.as_deref() == Some(expected));
    let mut issues = Vec::new();
    if shim_installed && !shim_managed {
        issues.push("shim exists but is not za-managed".to_string());
    } else if shim_installed && !shim_current {
        issues.push("shim is za-managed but points to an older za path or content".to_string());
    }
    if za_executable.is_none() {
        issues.push("cannot resolve the current za executable path".to_string());
    }
    let legacy_shim_path =
        ide_agent_shim_path(&paths::legacy_jetbrains_agent_shim_bin_dir()?, &agent);
    if read_optional_file_lossy(&legacy_shim_path)?
        .as_deref()
        .is_some_and(ide_agent_shim_is_managed)
    {
        issues.push(format!(
            "legacy shim exists at `{}`; `za ide agent install {agent}` will clean it up",
            legacy_shim_path.display()
        ));
    }

    let probe_result = if probe {
        let probe = probe_jetbrains_agent_command(&agent);
        if !probe.success {
            issues.push(format!(
                "probe failed with exit code {:?}: {}",
                probe.exit_code, probe.stderr
            ));
        } else if probe.resolved.as_deref() != Some(&shim_path.display().to_string()) {
            issues.push(format!(
                "probe resolved `{agent}` to `{}` instead of `{}`",
                probe.resolved.as_deref().unwrap_or("unresolved"),
                shim_path.display()
            ));
        }
        Some(probe)
    } else {
        None
    };

    Ok(IdeAgentStatus {
        agent,
        shim_path: shim_path.display().to_string(),
        shim_installed,
        shim_managed,
        shim_current,
        run_target: run_target.map(|path| path.display().to_string()),
        probe: probe_result,
        issues,
    })
}

fn normalize_ide_agent_name(agent: &str) -> Result<String> {
    let agent = agent.trim();
    if agent.is_empty() {
        bail!("agent name cannot be empty");
    }
    let agent = crate::command::tool::canonical_tool_name(agent);
    if agent.starts_with('-')
        || !agent
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!("unsupported agent name `{agent}`; use a command name such as `codex`");
    }
    Ok(agent)
}

fn ide_agent_shim_path(shim_dir: &Path, agent: &str) -> PathBuf {
    shim_dir.join(agent)
}

fn expected_ide_agent_shim_content(
    agent: &str,
    za_executable: &Path,
    fallback_target: &Path,
) -> String {
    format!(
        r#"#!/usr/bin/env bash
{IDE_AGENT_SHIM_MANAGED_MARKER}
za_agent={agent}
za_bin={za}
za_fallback={fallback}

za_readlink() {{
  readlink -f -- "$1" 2>/dev/null || printf '%s\n' "$1"
}}

za_is_direct_jetbrains_agent_launch() {{
  [ "${{TERMINAL_EMULATOR-}}" = "JetBrains-JediTerm" ] || return 1
  [ -r "/proc/${{PPID}}/cmdline" ] || return 1
  za_parent_cmd="$(tr '\0' ' ' < "/proc/${{PPID}}/cmdline" 2>/dev/null || true)"
  case "$za_parent_cmd" in *serverMode*) ;; *) return 1 ;; esac
  case "$za_parent_cmd" in
    *JetBrains*|*rustrover*|*idea*|*pycharm*|*webstorm*|*goland*|*clion*) return 0 ;;
  esac
  return 1
}}

za_exec_passthrough() {{
  za_self="$(za_readlink "${{BASH_SOURCE[0]:-$0}}")"
  old_ifs="$IFS"
  IFS=:
  for za_dir in $PATH; do
    [ -n "$za_dir" ] || za_dir=.
    za_candidate="$za_dir/$za_agent"
    [ -x "$za_candidate" ] || continue
    [ "$(za_readlink "$za_candidate")" = "$za_self" ] && continue
    IFS="$old_ifs"
    exec "$za_candidate" "$@"
  done
  IFS="$old_ifs"
  if [ -x "$za_fallback" ] && [ "$(za_readlink "$za_fallback")" != "$za_self" ]; then
    exec "$za_fallback" "$@"
  fi
  printf '%s\n' "za: cannot find real $za_agent executable after JetBrains shim" >&2
  exit 127
}}

if za_is_direct_jetbrains_agent_launch; then
  exec "$za_bin" run "$za_agent" "$@"
fi

za_exec_passthrough "$@"
"#,
        agent = shell_single_quote(agent),
        za = shell_single_quote(&za_executable.display().to_string()),
        fallback = shell_single_quote(&fallback_target.display().to_string()),
    )
}

fn ide_agent_shim_is_managed(content: &str) -> bool {
    content.contains(IDE_AGENT_SHIM_MANAGED_MARKER)
        || content
            .lines()
            .any(|line| line.trim_start().starts_with("exec za run "))
}

fn resolve_current_za_executable() -> Result<PathBuf> {
    let path = env::current_exe().context("resolve current za executable path")?;
    let current = if path.is_absolute() {
        path
    } else {
        env::current_dir()
            .map(|cwd| cwd.join(path))
            .context("resolve current working directory for za executable")?
    };

    Ok(resolve_za_executable_from_path(&current).unwrap_or(current))
}

fn resolve_za_executable_from_path(current_exe: &Path) -> Option<PathBuf> {
    let path_env = env::var_os("PATH")?;
    resolve_za_executable_from_path_env(current_exe, &path_env)
}

fn resolve_za_executable_from_path_env(
    current_exe: &Path,
    path_env: &std::ffi::OsStr,
) -> Option<PathBuf> {
    let current_canonical = fs::canonicalize(current_exe).ok();
    let mut same_as_current = None;

    for dir in env::split_paths(path_env) {
        let candidate = dir.join("za");
        if !is_executable_file(&candidate) {
            continue;
        }
        if path_matches(&candidate, current_exe, current_canonical.as_deref()) {
            same_as_current.get_or_insert(candidate);
            continue;
        }
        return Some(candidate);
    }

    same_as_current
}

fn path_matches(path: &Path, expected: &Path, expected_canonical: Option<&Path>) -> bool {
    if path == expected {
        return true;
    }
    let (Some(path_canonical), Some(expected_canonical)) =
        (fs::canonicalize(path).ok(), expected_canonical)
    else {
        return false;
    };
    path_canonical == expected_canonical
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn read_optional_file_lossy(path: &Path) -> Result<Option<String>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("read `{}`", path.display())),
    }
}

fn remove_managed_legacy_ide_agent_shim(agent: &str) -> Result<bool> {
    let legacy_dir = paths::legacy_jetbrains_agent_shim_bin_dir()?;
    let legacy_path = ide_agent_shim_path(&legacy_dir, agent);
    let Some(existing) = read_optional_file_lossy(&legacy_path)? else {
        return Ok(false);
    };
    if !ide_agent_shim_is_managed(&existing) {
        return Ok(false);
    }
    match fs::remove_file(&legacy_path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| format!("remove `{}`", legacy_path.display())),
    }
}

fn probe_jetbrains_agent_command(agent: &str) -> IdeAgentProbe {
    let bash_command = format!("command -v -- {}", shell_single_quote(agent));
    let command = format!(
        "TERMINAL_EMULATOR=JetBrains-JediTerm bash -c {}",
        shell_single_quote(&bash_command)
    );
    let probe_path = paths::home_dir()
        .map(|home| {
            format!(
                "{}:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin",
                home.join(".local/bin").display()
            )
        })
        .unwrap_or_else(|_| "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin".to_string());
    let output = Command::new("bash")
        .env("TERMINAL_EMULATOR", "JetBrains-JediTerm")
        .env("PATH", probe_path)
        .arg("-c")
        .arg(bash_command)
        .output();
    match output {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let resolved = stdout
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .next_back()
                .map(str::to_string);
            IdeAgentProbe {
                command,
                success: output.status.success(),
                resolved,
                exit_code: output.status.code(),
                stderr,
            }
        }
        Err(err) => IdeAgentProbe {
            command,
            success: false,
            resolved: None,
            exit_code: None,
            stderr: err.to_string(),
        },
    }
}

fn list_installed_ide_agent_names(shim_dir: &Path) -> Result<Vec<String>> {
    let entries = match fs::read_dir(shim_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("read `{}`", shim_dir.display())),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("read entry in `{}`", shim_dir.display()))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if normalize_ide_agent_name(name).is_ok_and(|normalized| normalized == name) {
            names.push(name.to_string());
        }
    }
    names.sort();
    Ok(names)
}

#[cfg(test)]
fn upsert_ide_agent_bash_block(target_path: &Path, shim_dir: &Path) -> Result<bool> {
    let existing = match fs::read_to_string(target_path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err).with_context(|| format!("read `{}`", target_path.display())),
    };
    let (remaining, _) = remove_ide_agent_bash_block_from_content(&existing, target_path)?;
    let block = format!(
        "{}\n{}\n{}",
        IDE_AGENT_BASH_START_MARKER,
        ide_agent_bash_path_block(shim_dir),
        IDE_AGENT_BASH_END_MARKER
    );
    let updated = if remaining.trim().is_empty() {
        format!("{block}\n")
    } else {
        format!("{}\n\n{block}\n", remaining.trim_end())
    };
    if updated == existing {
        return Ok(false);
    }
    fs::write(target_path, updated)
        .with_context(|| format!("write `{}`", target_path.display()))?;
    Ok(true)
}

fn remove_ide_agent_bash_block(target_path: &Path) -> Result<bool> {
    let existing = match fs::read_to_string(target_path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err).with_context(|| format!("read `{}`", target_path.display())),
    };
    let (updated, removed) = remove_ide_agent_bash_block_from_content(&existing, target_path)?;
    if removed {
        fs::write(target_path, updated)
            .with_context(|| format!("write `{}`", target_path.display()))?;
    }
    Ok(removed)
}

fn ide_agent_bash_block_is_configured(target_path: &Path, _shim_dir: &Path) -> Result<bool> {
    let existing = match fs::read_to_string(target_path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err).with_context(|| format!("read `{}`", target_path.display())),
    };
    Ok(existing.contains(IDE_AGENT_BASH_START_MARKER)
        && existing.contains(IDE_AGENT_BASH_END_MARKER))
}

fn remove_ide_agent_bash_block_from_content(
    existing: &str,
    target_path: &Path,
) -> Result<(String, bool)> {
    let Some(start) = existing.find(IDE_AGENT_BASH_START_MARKER) else {
        return Ok((existing.to_string(), false));
    };
    let end = existing[start..]
        .find(IDE_AGENT_BASH_END_MARKER)
        .map(|offset| start + offset + IDE_AGENT_BASH_END_MARKER.len())
        .ok_or_else(|| {
            anyhow!(
                "found `{}` in `{}` without matching `{}`",
                IDE_AGENT_BASH_START_MARKER,
                target_path.display(),
                IDE_AGENT_BASH_END_MARKER
            )
        })?;
    let prefix = existing[..start].trim_end_matches('\n');
    let suffix = existing[end..].trim_start_matches('\n');
    let updated = match (prefix.is_empty(), suffix.is_empty()) {
        (true, true) => String::new(),
        (true, false) => format!("{suffix}\n"),
        (false, true) => format!("{prefix}\n"),
        (false, false) => format!("{prefix}\n\n{suffix}\n"),
    };
    Ok((updated, true))
}

#[cfg(test)]
fn ide_agent_bash_path_block(shim_dir: &Path) -> String {
    format!(
        r#"if [ "${{TERMINAL_EMULATOR-}}" = "JetBrains-JediTerm" ]; then
  za_ide_agent_shim_dir={}
  case ":${{PATH}}:" in
    *":${{za_ide_agent_shim_dir}}:"*) ;;
    *) export PATH="${{za_ide_agent_shim_dir}}:${{PATH}}" ;;
  esac
  unset za_ide_agent_shim_dir
fi"#,
        shell_single_quote(&shim_dir.display().to_string())
    )
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
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
        ProcessIdentity, expected_ide_agent_shim_content, ide_agent_bash_path_block,
        ide_agent_shim_is_managed, normalize_ide_agent_name, parse_ipcs_semaphore_rows,
        parse_toolbox_ipc_key_line, pick_keep_pid, pick_keep_pids, process_matches_identity,
        project_row_attention_rank, read_process_start_ticks, remove_ide_agent_bash_block,
        render_fix_lines, resolve_za_executable_from_path_env, shell_single_quote,
        sort_project_rows_for_ps, upsert_ide_agent_bash_block,
    };
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::{fs, path::PathBuf, time::SystemTime};

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
    fn normalize_ide_agent_name_accepts_safe_aliases() {
        assert_eq!(
            normalize_ide_agent_name("codex-cli").expect("valid alias"),
            "codex"
        );
        assert_eq!(
            normalize_ide_agent_name("claude-code").expect("valid name"),
            "claude-code"
        );
        assert!(normalize_ide_agent_name("../codex").is_err());
        assert!(normalize_ide_agent_name("-codex").is_err());
    }

    #[test]
    fn ide_agent_shim_execs_za_run_agent() {
        let content = expected_ide_agent_shim_content(
            "codex",
            PathBuf::from("/usr/bin/za").as_path(),
            PathBuf::from("/usr/local/bin/codex").as_path(),
        );
        assert!(content.contains("# za-managed: ide-agent-shim v1"));
        assert!(content.contains("exec \"$za_bin\" run \"$za_agent\" \"$@\""));
        assert!(content.contains("za_fallback='/usr/local/bin/codex'"));
        assert!(content.contains("za_is_direct_jetbrains_agent_launch"));
        assert!(content.contains(r#"case "$za_parent_cmd" in *serverMode*)"#));
        assert!(ide_agent_shim_is_managed(&content));
        assert!(ide_agent_shim_is_managed(
            "#!/usr/bin/env bash\nexec za run codex \"$@\"\n"
        ));
        assert!(!ide_agent_shim_is_managed(
            "#!/usr/bin/env bash\nexec codex \"$@\"\n"
        ));
    }

    #[test]
    fn ide_agent_bash_path_block_is_jetbrains_scoped() {
        let block = ide_agent_bash_path_block(PathBuf::from("/tmp/za shims").as_path());
        assert!(block.contains(r#""${TERMINAL_EMULATOR-}" = "JetBrains-JediTerm""#));
        assert!(block.contains("za_ide_agent_shim_dir='/tmp/za shims'"));
        assert!(block.contains("export PATH="));
    }

    #[test]
    fn shell_single_quote_escapes_single_quotes() {
        assert_eq!(shell_single_quote("/tmp/a'b"), "'/tmp/a'\"'\"'b'");
    }

    #[test]
    fn resolve_za_executable_from_path_prefers_installed_za_over_current_exe() {
        let dir = TempDir::new("ide-agent-za-path").expect("temp dir");
        let current_dir = dir.path.join("target/debug");
        let installed_dir = dir.path.join("bin");
        fs::create_dir_all(&current_dir).expect("create current dir");
        fs::create_dir_all(&installed_dir).expect("create installed dir");
        let current = current_dir.join("za");
        let installed = installed_dir.join("za");
        fs::write(&current, "#!/bin/sh\n").expect("write current");
        fs::write(&installed, "#!/bin/sh\n").expect("write installed");
        #[cfg(unix)]
        {
            fs::set_permissions(&current, fs::Permissions::from_mode(0o755))
                .expect("chmod current");
            fs::set_permissions(&installed, fs::Permissions::from_mode(0o755))
                .expect("chmod installed");
        }

        let path = std::env::join_paths([current_dir, installed_dir]).expect("join path");
        let resolved =
            resolve_za_executable_from_path_env(&current, &path).expect("resolve installed za");

        assert_eq!(resolved, installed);
    }

    #[test]
    fn ide_agent_bash_block_upsert_and_remove_round_trip() {
        let dir = TempDir::new("ide-agent-bashrc").expect("temp dir");
        let rc_path = dir.path.join(".bashrc");
        fs::write(&rc_path, "before\n").expect("write rc");
        let shim_dir = dir.path.join("shims");

        assert!(upsert_ide_agent_bash_block(&rc_path, &shim_dir).expect("upsert"));
        let first = fs::read_to_string(&rc_path).expect("read rc");
        assert!(first.contains("before"));
        assert!(first.contains("za ide agent shims"));
        assert!(!upsert_ide_agent_bash_block(&rc_path, &shim_dir).expect("upsert unchanged"));
        assert!(remove_ide_agent_bash_block(&rc_path).expect("remove"));
        assert_eq!(fs::read_to_string(&rc_path).expect("read rc"), "before\n");
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

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(prefix: &str) -> std::io::Result<Self> {
            let unique = SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("za-test-{prefix}-{}-{unique}", std::process::id()));
            fs::create_dir_all(&path)?;
            Ok(Self { path })
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
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
