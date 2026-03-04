use super::{
    RemoteProjectState, RemoteSessionState,
    project_state::{current_unix_millis, jetbrains_config_dir, normalize_project_path},
};
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream},
    path::{Path, PathBuf},
    time::Duration,
};

const TOOLBOX_STATUS_CONNECT_TIMEOUT_MS: u64 = 350;
const TOOLBOX_STATUS_IO_TIMEOUT_MS: u64 = 1200;
const TOOLBOX_STATUS_PATH: &str = "/api/toolbox/status";

#[derive(Debug, Default)]
struct HttpLocalResponse {
    status_code: u16,
    body: Vec<u8>,
}

#[derive(Debug, Default)]
struct ToolboxStatusSessionState {
    backend_unresponsive: bool,
    modal_dialog_is_opened: bool,
    ide_identity_string: Option<String>,
    freshest_snapshot_millis: u128,
    projects: Vec<RemoteProjectState>,
}

#[derive(Debug, Deserialize)]
struct ToolboxStatusResponse {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    pid: Option<i32>,
    #[serde(rename = "ideStatus", default)]
    ide_status: Option<ToolboxIdeStatus>,
}

#[derive(Debug, Deserialize)]
struct ToolboxIdeStatus {
    #[serde(default)]
    pid: Option<i32>,
    #[serde(rename = "isUiThreadResponsive", default)]
    is_ui_thread_responsive: bool,
    #[serde(rename = "isModalDialogOpen", default)]
    is_modal_dialog_open: bool,
    #[serde(rename = "isBackgroundTaskRunning", default)]
    is_background_task_running: bool,
    #[serde(rename = "secondSinceLastUserActivity", default)]
    seconds_since_last_user_activity: Option<u64>,
    #[serde(rename = "openProjects", default)]
    open_projects: Vec<ToolboxOpenProject>,
}

#[derive(Debug, Deserialize)]
struct ToolboxOpenProject {
    #[serde(default)]
    path: Option<String>,
}

pub(super) fn merge_toolbox_status_state(state_by_pid: &mut HashMap<i32, RemoteSessionState>) {
    let status_by_pid = load_toolbox_status_session_state_by_pid();
    for (pid, status) in status_by_pid {
        let current = state_by_pid.entry(pid).or_default();
        if status.freshest_snapshot_millis >= current.freshest_snapshot_millis {
            current.backend_unresponsive = status.backend_unresponsive;
            current.modal_dialog_is_opened = status.modal_dialog_is_opened;
            if status.ide_identity_string.is_some() {
                current.ide_identity_string = status.ide_identity_string;
            }
            current.freshest_snapshot_millis = status.freshest_snapshot_millis;
        }
        if !status.projects.is_empty() {
            current.projects = merge_status_projects(status.projects, &current.projects);
            current
                .projects
                .sort_by(|a, b| a.project_path.cmp(&b.project_path));
        }
    }
}

fn merge_status_projects(
    mut status_projects: Vec<RemoteProjectState>,
    recent_projects: &[RemoteProjectState],
) -> Vec<RemoteProjectState> {
    let mut recent_by_path: HashMap<String, &RemoteProjectState> = HashMap::new();
    for project in recent_projects {
        match recent_by_path.get(&project.project_path) {
            Some(existing) if existing.snapshot_millis > project.snapshot_millis => {}
            _ => {
                recent_by_path.insert(project.project_path.clone(), project);
            }
        }
    }
    for project in &mut status_projects {
        if let Some(recent) = recent_by_path.get(&project.project_path) {
            if project.date_last_opened_ms.is_none() {
                project.date_last_opened_ms = recent.date_last_opened_ms;
            }
            if project.users.is_empty() {
                project.users = recent.users.clone();
            }
        }
    }
    status_projects
}

fn load_toolbox_status_session_state_by_pid() -> HashMap<i32, ToolboxStatusSessionState> {
    let Some(config_dir) = jetbrains_config_dir() else {
        return HashMap::new();
    };
    let identity_entries = match fs::read_dir(config_dir) {
        Ok(entries) => entries,
        Err(_) => return HashMap::new(),
    };
    let mut out = HashMap::new();
    for identity_entry in identity_entries.flatten() {
        let identity_dir = identity_entry.path();
        if !identity_dir.is_dir() {
            continue;
        }
        let ide_identity = normalize_project_path(&identity_dir.display().to_string());
        let vmoptions_entries = match fs::read_dir(&identity_dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for vmoptions_entry in vmoptions_entries.flatten() {
            let vmoptions_path = vmoptions_entry.path();
            if !vmoptions_path.is_file() {
                continue;
            }
            let file_name = vmoptions_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if !file_name.ends_with(".vmoptions") {
                continue;
            }
            let Some((token, port_file)) =
                parse_toolbox_notification_from_vmoptions(&vmoptions_path)
            else {
                continue;
            };
            let Some(port) = read_toolbox_port_from_file(&port_file) else {
                continue;
            };
            let Some(status) = fetch_toolbox_status_via_local_http(port, &token) else {
                continue;
            };
            let Some((pid, state)) =
                map_toolbox_status_to_state(status, &ide_identity, current_unix_millis())
            else {
                continue;
            };
            let existing = out.entry(pid).or_default();
            merge_toolbox_session(existing, state);
        }
    }
    out
}

fn merge_toolbox_session(
    target: &mut ToolboxStatusSessionState,
    incoming: ToolboxStatusSessionState,
) {
    if incoming.freshest_snapshot_millis >= target.freshest_snapshot_millis {
        target.backend_unresponsive = incoming.backend_unresponsive;
        target.modal_dialog_is_opened = incoming.modal_dialog_is_opened;
        if incoming.ide_identity_string.is_some() {
            target.ide_identity_string = incoming.ide_identity_string;
        }
        target.freshest_snapshot_millis = incoming.freshest_snapshot_millis;
    }
    if !incoming.projects.is_empty() {
        let mut by_path = target
            .projects
            .iter()
            .map(|p| (p.project_path.clone(), p.clone()))
            .collect::<HashMap<_, _>>();
        for project in incoming.projects {
            match by_path.get(&project.project_path) {
                Some(existing) if existing.snapshot_millis > project.snapshot_millis => {}
                _ => {
                    by_path.insert(project.project_path.clone(), project);
                }
            }
        }
        target.projects = by_path.into_values().collect::<Vec<_>>();
        target
            .projects
            .sort_by(|a, b| a.project_path.cmp(&b.project_path));
    }
}

fn map_toolbox_status_to_state(
    status: ToolboxStatusResponse,
    ide_identity: &str,
    snapshot_millis: Option<u64>,
) -> Option<(i32, ToolboxStatusSessionState)> {
    let ide_status = status.ide_status?;
    if status
        .status
        .as_deref()
        .is_some_and(|state| !state.eq_ignore_ascii_case("accepted"))
    {
        return None;
    }
    let pid = status.pid.or(ide_status.pid)?;
    let snapshot_millis = u128::from(snapshot_millis?);

    let mut projects = Vec::new();
    for project in ide_status.open_projects {
        let Some(path) = project
            .path
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        projects.push(RemoteProjectState {
            project_path: normalize_project_path(path),
            connected: true,
            seconds_since_last_controller_activity: ide_status.seconds_since_last_user_activity,
            date_last_opened_ms: None,
            background_tasks_running: ide_status.is_background_task_running,
            users: Vec::new(),
            snapshot_millis,
        });
    }
    projects.sort_by(|a, b| a.project_path.cmp(&b.project_path));

    Some((
        pid,
        ToolboxStatusSessionState {
            backend_unresponsive: !ide_status.is_ui_thread_responsive,
            modal_dialog_is_opened: ide_status.is_modal_dialog_open,
            ide_identity_string: Some(ide_identity.to_string()),
            freshest_snapshot_millis: snapshot_millis,
            projects,
        },
    ))
}

fn parse_toolbox_notification_from_vmoptions(path: &Path) -> Option<(String, PathBuf)> {
    let raw = fs::read_to_string(path).ok()?;
    let mut token = None::<String>;
    let mut port_file = None::<PathBuf>;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(value) = line.strip_prefix("-Dtoolbox.notification.token=") {
            let value = value.trim();
            if !value.is_empty() {
                token = Some(value.to_string());
            }
            continue;
        }
        if let Some(value) = line.strip_prefix("-Dtoolbox.notification.portFile=") {
            let value = value.trim();
            if !value.is_empty() {
                port_file = Some(PathBuf::from(value));
            }
        }
    }
    Some((token?, port_file?))
}

fn read_toolbox_port_from_file(path: &Path) -> Option<u16> {
    let raw = fs::read_to_string(path).ok()?;
    raw.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .and_then(|line| line.parse::<u16>().ok())
}

fn fetch_toolbox_status_via_local_http(port: u16, token: &str) -> Option<ToolboxStatusResponse> {
    let response = query_local_http_status(port, token).ok()?;
    if response.status_code != 200 {
        return None;
    }
    serde_json::from_slice::<ToolboxStatusResponse>(&response.body).ok()
}

fn query_local_http_status(port: u16, token: &str) -> Result<HttpLocalResponse> {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let timeout = Duration::from_millis(TOOLBOX_STATUS_CONNECT_TIMEOUT_MS);
    let mut stream = TcpStream::connect_timeout(&addr, timeout)
        .with_context(|| format!("connect toolbox status endpoint at 127.0.0.1:{port}"))?;
    let io_timeout = Some(Duration::from_millis(TOOLBOX_STATUS_IO_TIMEOUT_MS));
    let _ = stream.set_read_timeout(io_timeout);
    let _ = stream.set_write_timeout(io_timeout);
    let request = format!(
        "GET {TOOLBOX_STATUS_PATH} HTTP/1.1\r\n\
Host: 127.0.0.1:{port}\r\n\
Authorization: toolbox {token}\r\n\
Accept: application/json\r\n\
Connection: close\r\n\
\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .context("write toolbox status HTTP request")?;
    let mut raw = Vec::new();
    let mut buf = [0_u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(read) => raw.extend_from_slice(&buf[..read]),
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(err) => return Err(err).context("read toolbox status HTTP response"),
        }
    }
    if raw.is_empty() {
        bail!("empty toolbox status HTTP response");
    }
    decode_http_local_response(&raw).ok_or_else(|| anyhow!("parse toolbox HTTP response"))
}

fn decode_http_local_response(raw: &[u8]) -> Option<HttpLocalResponse> {
    let header_end = raw.windows(4).position(|window| window == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&raw[..header_end]).ok()?;
    let body = &raw[header_end + 4..];

    let mut lines = head.split("\r\n");
    let status_line = lines.next()?;
    let status_code = status_line.split_whitespace().nth(1)?.parse::<u16>().ok()?;
    let mut headers = HashMap::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }
    let body = decode_http_response_body(&headers, body)?;
    Some(HttpLocalResponse { status_code, body })
}

fn decode_http_response_body(headers: &HashMap<String, String>, body: &[u8]) -> Option<Vec<u8>> {
    if headers
        .get("transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"))
    {
        return decode_chunked_body(body);
    }
    if let Some(content_len) = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
    {
        if content_len > body.len() {
            return None;
        }
        return Some(body[..content_len].to_vec());
    }
    Some(body.to_vec())
}

fn decode_chunked_body(raw: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut cursor = 0usize;
    loop {
        let line_end = raw
            .get(cursor..)?
            .windows(2)
            .position(|window| window == b"\r\n")?
            + cursor;
        let size_line = std::str::from_utf8(raw.get(cursor..line_end)?).ok()?;
        let size = usize::from_str_radix(size_line.split(';').next()?.trim(), 16).ok()?;
        cursor = line_end + 2;
        if size == 0 {
            break;
        }
        let chunk = raw.get(cursor..cursor + size)?;
        out.extend_from_slice(chunk);
        cursor = cursor.checked_add(size + 2)?;
    }
    Some(out)
}
