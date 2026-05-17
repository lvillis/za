use super::{
    ChildProc, DEFAULT_CLK_TCK, DEFAULT_PAGE_SIZE, IdeProvider, IdeSession, IdeVersionInfo,
    PROC_ROOT, ProcStat, ProductInfo, RemoteProjectSource, RemoteProjectState,
};
use super::{project_state, zed_rpc};
use anyhow::{Context, Result, anyhow, bail};
use std::{
    collections::{HashMap, HashSet},
    env, fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::Command,
};

pub(super) fn collect_ide_sessions() -> Result<Vec<IdeSession>> {
    let snapshot = collect_proc_snapshot()?;
    let mut sessions = collect_jetbrains_sessions_from_snapshot(&snapshot);
    sessions.extend(collect_zed_sessions_from_snapshot(&snapshot));
    finish_sessions(&mut sessions, &snapshot.child_map);
    Ok(sessions)
}

pub(super) fn collect_jetbrains_sessions() -> Result<Vec<IdeSession>> {
    let snapshot = collect_proc_snapshot()?;
    let mut sessions = collect_jetbrains_sessions_from_snapshot(&snapshot);
    finish_sessions(&mut sessions, &snapshot.child_map);
    Ok(sessions)
}

struct ProcScanSnapshot {
    system_uptime_secs: f64,
    clock_ticks: u64,
    page_size: u64,
    now_millis: Option<u64>,
    processes: Vec<ProcessRecord>,
    child_map: HashMap<i32, Vec<ChildProc>>,
}

struct ProcessRecord {
    pid: i32,
    proc_dir: PathBuf,
    stat: ProcStat,
    args: Vec<String>,
}

fn collect_proc_snapshot() -> Result<ProcScanSnapshot> {
    let proc_root = Path::new(PROC_ROOT);
    if !proc_root.exists() {
        bail!("`za ide` is only supported on Linux with /proc");
    }

    let system_uptime = read_system_uptime_secs()?;
    let clock_ticks = getconf_u64("CLK_TCK").unwrap_or(DEFAULT_CLK_TCK).max(1);
    let page_size = getconf_u64("PAGESIZE").unwrap_or(DEFAULT_PAGE_SIZE).max(1);
    let now_millis = project_state::current_unix_millis();

    let mut processes = Vec::new();
    let mut child_map: HashMap<i32, Vec<ChildProc>> = HashMap::new();
    for entry in fs::read_dir(proc_root).context("read /proc")? {
        let entry = match entry {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(pid) = parse_pid_dir_name(&entry.file_name().to_string_lossy()) else {
            continue;
        };
        let proc_dir = entry.path();
        let stat = match read_proc_stat(&proc_dir.join("stat")) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let comm = read_trimmed(&proc_dir.join("comm")).unwrap_or_default();
        let cmdline_raw = fs::read(proc_dir.join("cmdline")).unwrap_or_default();
        let args = if cmdline_raw.is_empty() {
            Vec::new()
        } else {
            parse_cmdline(&cmdline_raw)
        };
        child_map.entry(stat.ppid).or_default().push(ChildProc {
            comm: comm.clone(),
            shell_integration: has_jetbrains_shell_integration(&args),
        });
        processes.push(ProcessRecord {
            pid,
            proc_dir,
            stat,
            args,
        });
    }

    Ok(ProcScanSnapshot {
        system_uptime_secs: system_uptime,
        clock_ticks,
        page_size,
        now_millis,
        processes,
        child_map,
    })
}

fn collect_jetbrains_sessions_from_snapshot(snapshot: &ProcScanSnapshot) -> Vec<IdeSession> {
    let remote_state_by_pid = project_state::load_remote_session_state_by_pid();
    let mut sessions = Vec::new();
    let mut version_cache: HashMap<PathBuf, IdeVersionInfo> = HashMap::new();
    for process in &snapshot.processes {
        let Some((ide, executable, project_arg)) = extract_jetbrains_server_session(&process.args)
        else {
            continue;
        };

        let (cpu_percent, rss_bytes, uptime_secs) = process_resource_usage(process, snapshot);
        let uid = read_uid_from_status(&process.proc_dir.join("status")).unwrap_or(0);
        let remote_state = remote_state_by_pid
            .get(&process.pid)
            .cloned()
            .unwrap_or_default();
        let heap_limit_bytes =
            resolve_heap_limit_bytes(&executable, remote_state.ide_identity_string.as_deref());
        let remote_projects = remote_state.projects;
        let (project, project_real) =
            project_state::derive_project_identity(&project_arg, &remote_projects);
        let version_info = resolve_ide_version_info(&executable, &mut version_cache);
        sessions.push(IdeSession {
            provider: IdeProvider::JetBrains,
            pid: process.pid,
            ppid: process.stat.ppid,
            uid,
            ide,
            ide_version: version_info.version,
            ide_build_number: version_info.build_number,
            executable,
            project,
            project_real,
            cpu_percent,
            rss_bytes,
            heap_limit_bytes,
            uptime_secs,
            child_count: 0,
            fsnotifier_children: 0,
            shell_children: 0,
            remote_backend_unresponsive: remote_state.backend_unresponsive,
            remote_modal_dialog_is_opened: remote_state.modal_dialog_is_opened,
            remote_projects,
            remote_ide_identity: remote_state.ide_identity_string,
            remote_snapshot_millis: remote_state.freshest_snapshot_millis,
            remote_snapshot_age_secs: project_state::snapshot_age_secs(
                snapshot.now_millis,
                remote_state.freshest_snapshot_millis,
            ),
            ide_station_socket_live: ide_station_socket_exists(uid, process.pid),
            remote_rpc_responsive: None,
            duplicate_group_size: 1,
            over_limit: false,
            orphan: process.stat.ppid == 1,
            orphan_due: false,
            start_ticks: process.stat.start_ticks,
        });
    }
    sessions
}

fn process_resource_usage(process: &ProcessRecord, snapshot: &ProcScanSnapshot) -> (f64, u64, u64) {
    let elapsed = process_elapsed_secs(
        snapshot.system_uptime_secs,
        process.stat.start_ticks,
        snapshot.clock_ticks,
    );
    let cpu_percent = process_cpu_percent(
        process
            .stat
            .utime_ticks
            .saturating_add(process.stat.stime_ticks),
        snapshot.clock_ticks,
        elapsed,
    );
    let uptime_secs = elapsed.max(0.0) as u64;
    let rss_pages = u64::try_from(process.stat.rss_pages.max(0)).unwrap_or_default();
    (
        cpu_percent,
        rss_pages.saturating_mul(snapshot.page_size),
        uptime_secs,
    )
}

#[derive(Debug)]
struct ZedRunSession {
    executable: String,
    workspace_id: Option<String>,
    log_file: Option<PathBuf>,
    pid_file: Option<PathBuf>,
    stdin_socket: Option<PathBuf>,
    stdout_socket: Option<PathBuf>,
    stderr_socket: Option<PathBuf>,
}

fn collect_zed_sessions_from_snapshot(snapshot: &ProcScanSnapshot) -> Vec<IdeSession> {
    let mut sessions = Vec::new();
    let occupied_workspaces = snapshot
        .processes
        .iter()
        .filter_map(|process| extract_zed_proxy_workspace(&process.args))
        .collect::<HashSet<_>>();
    for process in &snapshot.processes {
        let Some(zed) = extract_zed_run_session(&process.args) else {
            continue;
        };
        if !zed_pid_file_matches_process(zed.pid_file.as_deref(), process.pid) {
            continue;
        }

        let (cpu_percent, rss_bytes, uptime_secs) = process_resource_usage(process, snapshot);
        let uid = read_uid_from_status(&process.proc_dir.join("status")).unwrap_or(0);
        let transport_occupied = zed
            .workspace_id
            .as_ref()
            .is_some_and(|workspace| occupied_workspaces.contains(workspace));
        let rpc_probe = if transport_occupied {
            None
        } else {
            Some(zed_rpc::probe(
                zed.stdin_socket.as_deref(),
                zed.stdout_socket.as_deref(),
                zed.stderr_socket.as_deref(),
            ))
        };
        let mut rpc_projects = Vec::new();
        if let Some(probe) = &rpc_probe {
            for project in &probe.projects {
                push_unique_path(
                    &mut rpc_projects,
                    project_state::normalize_project_path(project),
                );
            }
        }
        let log_project = zed.log_file.as_deref().and_then(infer_zed_project_from_log);
        let (project_paths, project_source) =
            resolve_zed_project_paths(rpc_projects, log_project, zed.workspace_id.as_deref());
        let project_real = match project_paths.as_slice() {
            [single] => single.clone(),
            _ => "<multi-project>".to_string(),
        };
        let snapshot_millis = snapshot.now_millis.map(u128::from).unwrap_or_default();
        let control_live = zed_control_sockets_live(&zed);
        let remote_projects = project_paths
            .into_iter()
            .map(|project_path| RemoteProjectState {
                project_path,
                source: project_source,
                connected: control_live,
                seconds_since_last_controller_activity: control_live.then_some(0),
                date_last_opened_ms: None,
                background_tasks_running: false,
                users: Vec::new(),
                snapshot_millis,
            })
            .collect();

        sessions.push(IdeSession {
            provider: IdeProvider::Zed,
            pid: process.pid,
            ppid: process.stat.ppid,
            uid,
            ide: "zed".to_string(),
            ide_version: parse_zed_remote_version(&zed.executable),
            ide_build_number: None,
            executable: zed.executable,
            project: project_real.clone(),
            project_real,
            cpu_percent,
            rss_bytes,
            heap_limit_bytes: None,
            uptime_secs,
            child_count: 0,
            fsnotifier_children: 0,
            shell_children: 0,
            remote_backend_unresponsive: false,
            remote_modal_dialog_is_opened: false,
            remote_projects,
            remote_ide_identity: zed.workspace_id,
            remote_snapshot_millis: snapshot_millis,
            remote_snapshot_age_secs: project_state::snapshot_age_secs(
                snapshot.now_millis,
                snapshot_millis,
            ),
            ide_station_socket_live: control_live,
            remote_rpc_responsive: rpc_probe.as_ref().map(|probe| probe.responsive),
            duplicate_group_size: 1,
            over_limit: false,
            orphan: process.stat.ppid == 1,
            orphan_due: false,
            start_ticks: process.stat.start_ticks,
        });
    }
    sessions
}

fn resolve_zed_project_paths(
    rpc_projects: Vec<String>,
    log_project: Option<String>,
    workspace_id: Option<&str>,
) -> (Vec<String>, RemoteProjectSource) {
    if !rpc_projects.is_empty() {
        return (rpc_projects, RemoteProjectSource::Rpc);
    }
    if let Some(project) = log_project {
        return (vec![project], RemoteProjectSource::Log);
    }
    (
        vec![zed_unknown_project_label(workspace_id)],
        RemoteProjectSource::Unknown,
    )
}

fn zed_unknown_project_label(workspace_id: Option<&str>) -> String {
    workspace_id
        .map(|workspace| format!("<unknown:{workspace}>"))
        .unwrap_or_else(|| "<unknown>".to_string())
}

fn extract_zed_run_session(args: &[String]) -> Option<ZedRunSession> {
    let executable = args.first()?.clone();
    if !looks_like_zed_remote_server(&executable) {
        return None;
    }
    if !args.iter().any(|arg| arg == "run") {
        return None;
    }
    let log_file = option_path(args, "--log-file");
    let pid_file = option_path(args, "--pid-file");
    let stdin_socket = option_path(args, "--stdin-socket");
    let stdout_socket = option_path(args, "--stdout-socket");
    let stderr_socket = option_path(args, "--stderr-socket");
    let workspace_id = zed_workspace_id(pid_file.as_deref(), log_file.as_deref());
    Some(ZedRunSession {
        executable,
        workspace_id,
        log_file,
        pid_file,
        stdin_socket,
        stdout_socket,
        stderr_socket,
    })
}

fn extract_zed_proxy_workspace(args: &[String]) -> Option<String> {
    let executable = args.first()?;
    if !looks_like_zed_remote_server(executable) || !args.iter().any(|arg| arg == "proxy") {
        return None;
    }
    option_value(args, "--identifier").map(ToString::to_string)
}

fn looks_like_zed_remote_server(executable: &str) -> bool {
    Path::new(executable)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.contains("zed-remote-server"))
}

fn option_path(args: &[String], name: &str) -> Option<PathBuf> {
    option_value(args, name).map(PathBuf::from)
}

fn option_value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    for (index, arg) in args.iter().enumerate() {
        if arg == name {
            return args.get(index + 1).map(String::as_str);
        }
        if let Some(value) = arg.strip_prefix(&format!("{name}="))
            && !value.is_empty()
        {
            return Some(value);
        }
    }
    None
}

fn zed_workspace_id(pid_file: Option<&Path>, log_file: Option<&Path>) -> Option<String> {
    if let Some(parent) = pid_file.and_then(Path::parent)
        && let Some(name) = parent.file_name().and_then(|name| name.to_str())
        && name.starts_with("workspace-")
    {
        return Some(name.to_string());
    }
    let name = log_file?.file_name()?.to_str()?;
    let workspace = name
        .strip_prefix("server-")
        .and_then(|value| value.strip_suffix(".log"))?;
    if workspace.starts_with("workspace-") {
        return Some(workspace.to_string());
    }
    None
}

fn zed_pid_file_matches_process(pid_file: Option<&Path>, pid: i32) -> bool {
    let Some(pid_file) = pid_file else {
        return true;
    };
    let Ok(raw) = fs::read_to_string(pid_file) else {
        return true;
    };
    match raw.trim().parse::<i32>() {
        Ok(recorded_pid) => recorded_pid == pid,
        Err(_) => true,
    }
}

fn zed_control_sockets_live(session: &ZedRunSession) -> bool {
    [
        session.stdin_socket.as_deref(),
        session.stdout_socket.as_deref(),
        session.stderr_socket.as_deref(),
    ]
    .into_iter()
    .all(|path| path.is_some_and(Path::exists))
}

fn push_unique_path(paths: &mut Vec<String>, path: String) {
    if path.trim().is_empty() || paths.iter().any(|existing| existing == &path) {
        return;
    }
    paths.push(path);
}

fn parse_zed_remote_version(executable: &str) -> Option<String> {
    let name = Path::new(executable).file_name()?.to_str()?;
    name.split('-')
        .find(|part| part.chars().next().is_some_and(|ch| ch.is_ascii_digit()))
        .and_then(|part| part.split('+').next())
        .filter(|version| !version.is_empty())
        .map(ToString::to_string)
}

fn infer_zed_project_from_log(path: &Path) -> Option<String> {
    let raw = read_tail_lossy(path, 2 * 1024 * 1024).ok()?;
    parse_zed_project_from_log(&raw).map(|project| project_state::normalize_project_path(&project))
}

fn parse_zed_project_from_log(raw: &str) -> Option<String> {
    for line in raw.lines().rev() {
        let Some(path) = extract_quoted_after_marker(line, "opening git repository at ") else {
            continue;
        };
        if let Some(project) = path.strip_suffix("/.git") {
            return Some(project.to_string());
        }
        return Some(path);
    }

    for line in raw.lines().rev() {
        let Some(path) = extract_quoted_after_marker(line, "working directory: ") else {
            continue;
        };
        if is_plausible_zed_project_path(&path) {
            return Some(path);
        }
    }
    None
}

fn extract_quoted_after_marker(line: &str, marker: &str) -> Option<String> {
    let rest = line.get(line.find(marker)? + marker.len()..)?;
    if let Some(start) = rest.find("\\\"") {
        let value = rest.get(start + 2..)?;
        let end = value.find("\\\"")?;
        return Some(value[..end].replace("\\/", "/"));
    }
    let start = rest.find('"')?;
    let value = rest.get(start + 1..)?;
    let end = value.find('"')?;
    Some(value[..end].to_string())
}

fn is_plausible_zed_project_path(path: &str) -> bool {
    let project = Path::new(path);
    if !project.is_absolute() || project == Path::new("/") {
        return false;
    }
    if let Some(home) = env::var_os("HOME")
        && project == Path::new(&home)
    {
        return false;
    }
    let lowered = path.to_ascii_lowercase();
    !lowered.contains("/.local/share/zed/")
}

fn read_tail_lossy(path: &Path, max_bytes: u64) -> Result<String> {
    let mut file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let len = file.metadata()?.len();
    if len > max_bytes {
        file.seek(SeekFrom::Start(len - max_bytes))?;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

fn finish_sessions(sessions: &mut [IdeSession], child_map: &HashMap<i32, Vec<ChildProc>>) {
    for session in sessions.iter_mut() {
        if let Some(children) = child_map.get(&session.pid) {
            session.child_count = children.len();
            session.fsnotifier_children = children
                .iter()
                .filter(|child| child.comm.eq_ignore_ascii_case("fsnotifier"))
                .count();
            session.shell_children = children
                .iter()
                .filter(|child| is_shell_comm(&child.comm) && child.shell_integration)
                .count();
        }
    }
    sessions.sort_by(|a, b| {
        a.project_real
            .cmp(&b.project_real)
            .then_with(|| provider_rank(&a.provider).cmp(&provider_rank(&b.provider)))
            .then_with(|| a.ide.cmp(&b.ide))
            .then_with(|| a.pid.cmp(&b.pid))
    });
}

fn provider_rank(provider: &IdeProvider) -> u8 {
    match provider {
        IdeProvider::JetBrains => 0,
        IdeProvider::Zed => 1,
    }
}

fn parse_pid_dir_name(name: &str) -> Option<i32> {
    if name.chars().all(|c| c.is_ascii_digit()) {
        return name.parse::<i32>().ok();
    }
    None
}

fn read_proc_stat(path: &Path) -> Result<ProcStat> {
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    parse_proc_stat_line(&raw).ok_or_else(|| anyhow!("parse {}", path.display()))
}

pub(super) fn parse_proc_stat_line(raw: &str) -> Option<ProcStat> {
    let close_paren = raw.rfind(')')?;
    let tail = raw.get(close_paren + 1..)?.trim();
    let fields = tail.split_whitespace().collect::<Vec<_>>();
    let ppid = fields.get(1)?.parse::<i32>().ok()?;
    let utime_ticks = fields.get(11)?.parse::<u64>().ok()?;
    let stime_ticks = fields.get(12)?.parse::<u64>().ok()?;
    let start_ticks = fields.get(19)?.parse::<u64>().ok()?;
    let rss_pages = fields.get(21)?.parse::<i64>().ok()?;
    Some(ProcStat {
        ppid,
        utime_ticks,
        stime_ticks,
        start_ticks,
        rss_pages,
    })
}

pub(super) fn parse_cmdline(raw: &[u8]) -> Vec<String> {
    raw.split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).to_string())
        .collect()
}

pub(super) fn extract_jetbrains_server_session(
    args: &[String],
) -> Option<(String, String, String)> {
    let executable = args.first()?.clone();
    if !looks_like_jetbrains_executable(&executable) {
        return None;
    }
    let mode_index = args.iter().position(|arg| arg == "serverMode")?;
    let project = args.get(mode_index + 1).cloned().unwrap_or_default();
    let ide = Path::new(&executable)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("jetbrains-ide")
        .to_string();
    Some((ide, executable, project))
}

fn looks_like_jetbrains_executable(executable: &str) -> bool {
    let lower = executable.to_ascii_lowercase();
    if lower.contains("jetbrains") {
        return true;
    }
    let Some(name) = Path::new(executable).file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    const KNOWN: [&str; 12] = [
        "rustrover",
        "idea",
        "idea64",
        "pycharm",
        "clion",
        "webstorm",
        "goland",
        "datagrip",
        "phpstorm",
        "rubymine",
        "rider",
        "gateway",
    ];
    KNOWN.iter().any(|needle| name.contains(needle))
}

fn is_shell_comm(comm: &str) -> bool {
    matches!(
        comm.to_ascii_lowercase().as_str(),
        "bash" | "sh" | "zsh" | "fish" | "nu" | "xonsh" | "dash" | "ksh" | "tcsh" | "csh"
    )
}

pub(super) fn has_jetbrains_shell_integration(args: &[String]) -> bool {
    args.iter()
        .any(|arg| arg.contains("/plugins/terminal/shell-integrations/"))
}

fn runtime_dir_for_uid(uid: u32) -> PathBuf {
    if let Some(dir) = env::var_os("XDG_RUNTIME_DIR") {
        let from_env = PathBuf::from(dir);
        let expect = Path::new("/run/user").join(uid.to_string());
        if from_env == expect {
            return from_env;
        }
    }
    Path::new("/run/user").join(uid.to_string())
}

fn ide_station_socket_exists(uid: u32, pid: i32) -> bool {
    runtime_dir_for_uid(uid)
        .join(format!("jb.station.ij.{pid}.sock"))
        .exists()
}

fn resolve_ide_version_info(
    executable: &str,
    cache: &mut HashMap<PathBuf, IdeVersionInfo>,
) -> IdeVersionInfo {
    let Some(root) = ide_install_root_from_executable(executable) else {
        return IdeVersionInfo::default();
    };
    if let Some(cached) = cache.get(&root) {
        return cached.clone();
    }
    let info = read_ide_version_info(&root);
    cache.insert(root, info.clone());
    info
}

fn ide_install_root_from_executable(executable: &str) -> Option<PathBuf> {
    let exe = Path::new(executable);
    let parent = exe.parent()?;
    let maybe_bin = parent.file_name().and_then(|n| n.to_str());
    if maybe_bin == Some("bin") {
        return Some(parent.parent()?.to_path_buf());
    }
    Some(parent.to_path_buf())
}

fn read_ide_version_info(root: &Path) -> IdeVersionInfo {
    let mut info = IdeVersionInfo::default();
    let product_info_path = root.join("product-info.json");
    if let Ok(raw) = fs::read_to_string(product_info_path)
        && let Ok(product_info) = serde_json::from_str::<ProductInfo>(&raw)
    {
        info.version = normalize_non_empty(product_info.version);
        info.build_number = normalize_non_empty(product_info.build_number);
    }
    if info.build_number.is_none()
        && let Ok(raw) = fs::read_to_string(root.join("build.txt"))
    {
        info.build_number = parse_build_number_from_build_txt(&raw);
    }
    info
}

fn normalize_non_empty(value: Option<String>) -> Option<String> {
    let value = value?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

fn resolve_heap_limit_bytes(executable: &str, ide_identity: Option<&str>) -> Option<u64> {
    let identity_dir = Path::new(ide_identity?);
    let exe_name = Path::new(executable)
        .file_name()
        .and_then(|name| name.to_str())?
        .to_ascii_lowercase();
    let mut candidates = Vec::new();
    if exe_name.ends_with("64") {
        candidates.push(identity_dir.join(format!("{exe_name}.vmoptions")));
    } else {
        candidates.push(identity_dir.join(format!("{exe_name}64.vmoptions")));
        candidates.push(identity_dir.join(format!("{exe_name}.vmoptions")));
    }

    let mut seen = std::collections::HashSet::new();
    for path in candidates {
        if !seen.insert(path.clone()) {
            continue;
        }
        if let Some(bytes) = read_heap_limit_bytes_from_vmoptions(&path) {
            return Some(bytes);
        }
    }

    let mut fallback = Vec::new();
    for entry in fs::read_dir(identity_dir).ok()? {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "vmoptions") {
            fallback.push(path);
        }
    }
    fallback.sort();
    for path in fallback {
        if !seen.insert(path.clone()) {
            continue;
        }
        if let Some(bytes) = read_heap_limit_bytes_from_vmoptions(&path) {
            return Some(bytes);
        }
    }
    None
}

fn read_heap_limit_bytes_from_vmoptions(path: &Path) -> Option<u64> {
    let raw = fs::read_to_string(path).ok()?;
    raw.lines().filter_map(parse_xmx_bytes).next_back()
}

pub(super) fn parse_xmx_bytes(line: &str) -> Option<u64> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let value = line.strip_prefix("-Xmx")?;
    parse_jvm_size_bytes(value)
}

fn parse_jvm_size_bytes(value: &str) -> Option<u64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let digit_count = value.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if digit_count == 0 {
        return None;
    }
    let (digits, suffix) = value.split_at(digit_count);
    let base = digits.parse::<u64>().ok()?;
    let multiplier = match suffix.to_ascii_lowercase().as_str() {
        "" => 1,
        "k" => 1024,
        "m" => 1024_u64.pow(2),
        "g" => 1024_u64.pow(3),
        _ => return None,
    };
    base.checked_mul(multiplier)
}

pub(super) fn parse_build_number_from_build_txt(raw: &str) -> Option<String> {
    let first = raw.lines().find(|line| !line.trim().is_empty())?.trim();
    if let Some((_, maybe_build)) = first.split_once('-') {
        let build = maybe_build.trim();
        if !build.is_empty() {
            return Some(build.to_string());
        }
    }
    Some(first.to_string())
}

fn read_uid_from_status(path: &Path) -> Option<u32> {
    let raw = fs::read_to_string(path).ok()?;
    for line in raw.lines() {
        if !line.starts_with("Uid:") {
            continue;
        }
        let uid = line.split_whitespace().nth(1)?;
        return uid.parse::<u32>().ok();
    }
    None
}

fn read_system_uptime_secs() -> Result<f64> {
    let raw = fs::read_to_string("/proc/uptime").context("read /proc/uptime")?;
    let first = raw
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow!("invalid /proc/uptime"))?;
    first
        .parse::<f64>()
        .context("parse uptime seconds from /proc/uptime")
}

fn process_elapsed_secs(system_uptime_secs: f64, start_ticks: u64, clock_ticks: u64) -> f64 {
    let start_secs = start_ticks as f64 / clock_ticks as f64;
    (system_uptime_secs - start_secs).max(0.0)
}

fn process_cpu_percent(total_ticks: u64, clock_ticks: u64, elapsed_secs: f64) -> f64 {
    if elapsed_secs <= f64::EPSILON {
        return 0.0;
    }
    let cpu_secs = total_ticks as f64 / clock_ticks as f64;
    (cpu_secs / elapsed_secs) * 100.0
}

fn getconf_u64(name: &str) -> Option<u64> {
    let output = Command::new("getconf").arg(name).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout);
    value.trim().parse::<u64>().ok()
}

fn read_trimmed(path: &Path) -> Result<String> {
    Ok(fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?
        .trim()
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_zed_run_session_reads_workspace_and_paths() {
        let args = vec![
            "/root/.zed_server/zed-remote-server-stable-1.2.6+stable.280.abc".to_string(),
            "run".to_string(),
            "--log-file".to_string(),
            "/root/.local/share/zed/logs/server-workspace-9.log".to_string(),
            "--pid-file".to_string(),
            "/root/.local/share/zed/server_state/workspace-9/server.pid".to_string(),
            "--stdin-socket=/root/.local/share/zed/server_state/workspace-9/stdin.sock".to_string(),
            "--stdout-socket".to_string(),
            "/root/.local/share/zed/server_state/workspace-9/stdout.sock".to_string(),
            "--stderr-socket".to_string(),
            "/root/.local/share/zed/server_state/workspace-9/stderr.sock".to_string(),
        ];

        let session = extract_zed_run_session(&args).expect("zed run session");

        assert_eq!(session.workspace_id.as_deref(), Some("workspace-9"));
        assert_eq!(
            session.log_file.as_deref(),
            Some(Path::new(
                "/root/.local/share/zed/logs/server-workspace-9.log"
            ))
        );
        assert!(session.stdin_socket.is_some());
        assert!(session.stdout_socket.is_some());
        assert!(session.stderr_socket.is_some());
    }

    #[test]
    fn extract_zed_run_session_ignores_proxy_process() {
        let args = vec![
            ".zed_server/zed-remote-server-stable-1.2.6+stable.280.abc".to_string(),
            "proxy".to_string(),
            "--identifier".to_string(),
            "workspace-9".to_string(),
        ];

        assert!(extract_zed_run_session(&args).is_none());
    }

    #[test]
    fn extract_zed_proxy_workspace_reads_identifier() {
        let args = vec![
            ".zed_server/zed-remote-server-stable-1.2.6+stable.280.abc".to_string(),
            "proxy".to_string(),
            "--identifier".to_string(),
            "workspace-9".to_string(),
        ];

        assert_eq!(
            extract_zed_proxy_workspace(&args).as_deref(),
            Some("workspace-9")
        );
    }

    #[test]
    fn resolve_zed_project_paths_prefers_rpc_then_log_then_workspace_placeholder() {
        assert_eq!(
            resolve_zed_project_paths(vec!["/opt/app/live".to_string()], None, Some("workspace-9")),
            (vec!["/opt/app/live".to_string()], RemoteProjectSource::Rpc)
        );
        assert_eq!(
            resolve_zed_project_paths(Vec::new(), Some("/opt/app/log".to_string()), None),
            (vec!["/opt/app/log".to_string()], RemoteProjectSource::Log)
        );
        assert_eq!(
            resolve_zed_project_paths(Vec::new(), None, Some("workspace-9")),
            (
                vec!["<unknown:workspace-9>".to_string()],
                RemoteProjectSource::Unknown
            )
        );
    }

    #[test]
    fn parse_zed_remote_version_reads_semver_from_binary_name() {
        assert_eq!(
            parse_zed_remote_version(
                "/root/.zed_server/zed-remote-server-stable-1.2.6+stable.280.abc"
            )
            .as_deref(),
            Some("1.2.6")
        );
    }

    #[test]
    fn parse_zed_project_from_log_prefers_git_repository_root() {
        let raw = r#"{"message":"(remote server) starting language server process. binary path: \"/bin/node\", working directory: \"/root\", args: []"}
{"message":"(remote server) opening git repository at \"/opt/app/zed-jenkinsfile/.git\" using git binary \"/usr/bin/git\""}"#;

        assert_eq!(
            parse_zed_project_from_log(raw).as_deref(),
            Some("/opt/app/zed-jenkinsfile")
        );
    }

    #[test]
    fn parse_zed_project_from_log_falls_back_to_plausible_workdir() {
        let raw = r#"{"message":"(remote server) starting language server process. binary path: \"/bin/node\", working directory: \"/root\", args: []"}
{"message":"(remote server) starting language server process. binary path: \"/bin/rust-analyzer\", working directory: \"/opt/app/plain\", args: []"}"#;

        assert_eq!(
            parse_zed_project_from_log(raw).as_deref(),
            Some("/opt/app/plain")
        );
    }
}
