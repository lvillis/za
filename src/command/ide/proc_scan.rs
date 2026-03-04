use super::project_state;
use super::{
    ChildProc, DEFAULT_CLK_TCK, DEFAULT_PAGE_SIZE, IdeSession, IdeVersionInfo, PROC_ROOT, ProcStat,
    ProductInfo,
};
use anyhow::{Context, Result, anyhow, bail};
use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

pub(super) fn collect_ide_sessions() -> Result<Vec<IdeSession>> {
    let proc_root = Path::new(PROC_ROOT);
    if !proc_root.exists() {
        bail!("`za ide` is only supported on Linux with /proc");
    }

    let system_uptime = read_system_uptime_secs()?;
    let clock_ticks = getconf_u64("CLK_TCK").unwrap_or(DEFAULT_CLK_TCK).max(1);
    let page_size = getconf_u64("PAGESIZE").unwrap_or(DEFAULT_PAGE_SIZE).max(1);
    let remote_state_by_pid = project_state::load_remote_session_state_by_pid();
    let now_millis = project_state::current_unix_millis();

    let mut sessions = Vec::new();
    let mut child_map: HashMap<i32, Vec<ChildProc>> = HashMap::new();
    let mut version_cache: HashMap<PathBuf, IdeVersionInfo> = HashMap::new();
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
        if cmdline_raw.is_empty() {
            continue;
        }
        let Some((ide, executable, project_arg)) = extract_jetbrains_server_session(&args) else {
            continue;
        };

        let uid = read_uid_from_status(&proc_dir.join("status")).unwrap_or(0);
        let elapsed = process_elapsed_secs(system_uptime, stat.start_ticks, clock_ticks);
        let cpu_percent = process_cpu_percent(
            stat.utime_ticks.saturating_add(stat.stime_ticks),
            clock_ticks,
            elapsed,
        );
        let uptime_secs = elapsed.max(0.0) as u64;
        let rss_pages = u64::try_from(stat.rss_pages.max(0)).unwrap_or_default();
        let remote_state = remote_state_by_pid.get(&pid).cloned().unwrap_or_default();
        let remote_projects = remote_state.projects;
        let (project, project_real) =
            project_state::derive_project_identity(&project_arg, &remote_projects);
        let version_info = resolve_ide_version_info(&executable, &mut version_cache);
        sessions.push(IdeSession {
            pid,
            ppid: stat.ppid,
            uid,
            ide,
            ide_version: version_info.version,
            ide_build_number: version_info.build_number,
            executable,
            project,
            project_real,
            cpu_percent,
            rss_bytes: rss_pages.saturating_mul(page_size),
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
                now_millis,
                remote_state.freshest_snapshot_millis,
            ),
            ide_station_socket_live: ide_station_socket_exists(uid, pid),
            duplicate_group_size: 1,
            over_limit: false,
            orphan: stat.ppid == 1,
            orphan_due: false,
            start_ticks: stat.start_ticks,
        });
    }

    for session in &mut sessions {
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
            .then_with(|| a.ide.cmp(&b.ide))
            .then_with(|| a.pid.cmp(&b.pid))
    });
    Ok(sessions)
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
