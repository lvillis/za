//! Manage long-lived Codex work sessions backed by tmux.

use crate::cli::CodexCommands;
use anyhow::{Context, Result, anyhow, bail};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ignore::WalkBuilder;
use ratatui::{
    Terminal,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    env, fs,
    io::{self, BufRead, BufReader, ErrorKind, IsTerminal, Read, Seek, SeekFrom, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{self, Command},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const SESSION_PREFIX: &str = "za-codex";
const STATE_DIR_RELATIVE: &str = "za/codex/sessions";
const TOP_LISTENER_STATE_RELATIVE: &str = "za/codex/otel-listener.json";
const DEFAULT_WORKSPACE_LABEL: &str = "workspace";
const SESSION_HASH_LEN: usize = 12;
const SESSION_LABEL_MAX_LEN: usize = 24;
const TOP_LISTENER_STALE_SECS: u64 = 5;
const TOP_REFRESH_INTERVAL: Duration = Duration::from_millis(500);
const TOP_DISCOVERY_INTERVAL: Duration = Duration::from_secs(2);
const TOP_LISTENER_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
const TOP_STREAM_EVENT_CAP: usize = 256;
const MANAGED_TRACKER_MATCH_WINDOW_SECS: u64 = 600;

pub fn run(cmd: Option<CodexCommands>, passthrough_args: &[String]) -> Result<i32> {
    match cmd {
        Some(CodexCommands::Up { args }) => run_up(&args),
        Some(CodexCommands::Attach) => run_attach(),
        Some(CodexCommands::Exec { args }) => run_exec(&args),
        Some(CodexCommands::Resume { args }) => run_resume(&args),
        Some(CodexCommands::Ps { json }) => run_ps(json),
        Some(CodexCommands::Top { all, history }) => run_top(all, history),
        Some(CodexCommands::Stop { json }) => run_stop(json),
        None if passthrough_args.is_empty() => run_up(&[]),
        None => run_up_with_args(passthrough_args, true, "passthrough"),
    }
}

fn run_up(args: &[String]) -> Result<i32> {
    run_up_with_args(args, !args.is_empty(), "up")
}

fn run_up_with_args(args: &[String], force_recreate: bool, launcher: &str) -> Result<i32> {
    ensure_tmux_available()?;

    let ctx = resolve_workspace_context()?;
    let active_listener = top_listener_state_for_launch(args)?;
    if tmux_has_session(&ctx.session_name)? {
        if force_recreate {
            restart_managed_session(&ctx, launcher, args)?;
        } else if tmux_session_needs_top_listener_restart(
            &ctx.session_name,
            active_listener.as_ref(),
        )? {
            eprintln!(
                "Recreating managed Codex session `{}` to enable live OTLP streaming for `za codex top`.",
                ctx.session_name
            );
            tmux_kill_session(&ctx.session_name)?;
            remove_session_record(&ctx.metadata_path)?;
            start_managed_session(&ctx, CodexLaunchMode::Fresh, launcher, args)?;
        } else {
            persist_session_record(&ctx, launcher, args)?;
            return maybe_attach_or_report(&ctx.session_name, &ctx.workspace_root);
        }
    } else {
        start_managed_session(&ctx, CodexLaunchMode::Fresh, launcher, args)?;
    }
    maybe_attach_or_report(&ctx.session_name, &ctx.workspace_root)
}

fn run_resume(args: &[String]) -> Result<i32> {
    run_resume_with_args(args, !args.is_empty(), "resume")
}

fn run_resume_with_args(args: &[String], force_recreate: bool, launcher: &str) -> Result<i32> {
    ensure_tmux_available()?;

    let ctx = resolve_workspace_context()?;
    let active_listener = top_listener_state_for_launch(args)?;
    if tmux_has_session(&ctx.session_name)? {
        if force_recreate {
            restart_managed_resume_session(&ctx, launcher, args)?;
        } else if tmux_session_needs_top_listener_restart(
            &ctx.session_name,
            active_listener.as_ref(),
        )? {
            eprintln!(
                "Recreating managed Codex session `{}` to enable live OTLP streaming for `za codex top`.",
                ctx.session_name
            );
            tmux_kill_session(&ctx.session_name)?;
            remove_session_record(&ctx.metadata_path)?;
            start_managed_session(&ctx, CodexLaunchMode::ResumeLast, launcher, args)?;
        } else {
            persist_session_record(&ctx, launcher, args)?;
            return maybe_attach_or_report(&ctx.session_name, &ctx.workspace_root);
        }
    } else {
        start_managed_session(&ctx, CodexLaunchMode::ResumeLast, launcher, args)?;
    }
    maybe_attach_or_report(&ctx.session_name, &ctx.workspace_root)
}

fn restart_managed_session(ctx: &WorkspaceContext, launcher: &str, args: &[String]) -> Result<()> {
    eprintln!(
        "Recreating managed Codex session `{}` with explicit startup args.",
        ctx.session_name
    );
    tmux_kill_session(&ctx.session_name)?;
    remove_session_record(&ctx.metadata_path)?;
    start_managed_session(ctx, CodexLaunchMode::Fresh, launcher, args)
}

fn restart_managed_resume_session(
    ctx: &WorkspaceContext,
    launcher: &str,
    args: &[String],
) -> Result<()> {
    eprintln!(
        "Recreating managed Codex session `{}` with explicit resume args.",
        ctx.session_name
    );
    tmux_kill_session(&ctx.session_name)?;
    remove_session_record(&ctx.metadata_path)?;
    start_managed_session(ctx, CodexLaunchMode::ResumeLast, launcher, args)
}

fn start_managed_session(
    ctx: &WorkspaceContext,
    mode: CodexLaunchMode,
    launcher: &str,
    args: &[String],
) -> Result<()> {
    let command = build_codex_launch_command(mode, args)?;
    tmux_new_session(&ctx.session_name, &ctx.workspace_root, &command)?;
    tmux_ensure_outer_scrollback_preserved()?;
    tmux_disable_alternate_screen_for_codex_windows(&ctx.session_name)?;
    persist_session_record(ctx, launcher, args)?;
    Ok(())
}

fn run_attach() -> Result<i32> {
    ensure_tmux_available()?;

    let ctx = resolve_workspace_context()?;
    if !tmux_has_session(&ctx.session_name)? {
        bail!(
            "no managed Codex session for `{}`; start one with `za codex`",
            ctx.workspace_root.display()
        );
    }
    maybe_attach_or_report(&ctx.session_name, &ctx.workspace_root)
}

fn run_exec(args: &[String]) -> Result<i32> {
    ensure_tmux_available()?;

    if args.is_empty() {
        bail!("`za codex exec` requires a command after `--`");
    }

    let ctx = resolve_workspace_context()?;
    if !tmux_has_session(&ctx.session_name)? {
        bail!(
            "no managed Codex session for `{}`; start one with `za codex` first",
            ctx.workspace_root.display()
        );
    }

    let interactive = is_interactive_terminal();
    let window_name = next_exec_window_name();
    let command = build_exec_command(args)?;
    tmux_new_window(
        &ctx.session_name,
        &window_name,
        &ctx.workspace_root,
        &command,
        !interactive,
    )?;

    if interactive {
        attach_session(&ctx.session_name)
    } else {
        println!(
            "Started tmux window `{}` in session `{}` for {}.",
            window_name,
            ctx.session_name,
            ctx.workspace_root.display()
        );
        Ok(0)
    }
}

fn run_ps(json: bool) -> Result<i32> {
    let tmux_probe = probe_tmux()?;
    let tmux_available = matches!(tmux_probe, TmuxProbe::Available);
    let tmux_sessions = if tmux_available {
        list_tmux_sessions()?
    } else {
        BTreeMap::new()
    };
    let rows = collect_session_rows(&tmux_sessions, tmux_available)?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&CodexPsOutput {
                tmux_available,
                sessions: rows.clone()
            })
            .context("serialize codex ps output")?
        );
        return Ok(0);
    }

    if rows.is_empty() {
        println!("{}", no_managed_sessions_message(tmux_available));
        return Ok(0);
    }

    println!(
        "{:<36} {:<11} {:<8} {:<8} {:<12} {:<18} {:<10} {:<6} WORKSPACE",
        "SESSION", "STATUS", "CLIENTS", "ACTIVE", "ID", "MODEL", "EFFORT", "LEFT%"
    );
    for row in &rows {
        println!(
            "{:<36} {:<11} {:<8} {:<8} {:<12} {:<18} {:<10} {:<6} {}",
            truncate_end(&row.session_name, 36),
            row.status,
            row.attached_clients,
            activity_age_label(row.last_activity_unix),
            truncate_end(row.codex_session_id.as_deref().unwrap_or("-"), 12),
            truncate_end(row.codex_model.as_deref().unwrap_or("-"), 18),
            truncate_end(row.codex_effort.as_deref().unwrap_or("-"), 10),
            format_left_percent(row.context_left_percent),
            truncate_end(
                row.workspace_root
                    .as_deref()
                    .unwrap_or("<unknown workspace>"),
                72
            )
        );
    }
    Ok(0)
}

fn run_stop(json: bool) -> Result<i32> {
    let ctx = resolve_workspace_context()?;
    let metadata_present = ctx.metadata_path.exists();
    let output = match probe_tmux()? {
        TmuxProbe::Available => {
            let session_running = tmux_has_session(&ctx.session_name)?;
            if session_running {
                tmux_kill_session(&ctx.session_name)?;
            }
            remove_session_record(&ctx.metadata_path)?;
            CodexStopOutput {
                session_name: ctx.session_name,
                workspace_root: ctx.workspace_root.display().to_string(),
                stopped: session_running,
                metadata_removed: metadata_present,
                tmux_available: true,
                note: (!session_running).then_some("no running tmux session was found".to_string()),
            }
        }
        TmuxProbe::Missing => {
            remove_session_record(&ctx.metadata_path)?;
            CodexStopOutput {
                session_name: ctx.session_name,
                workspace_root: ctx.workspace_root.display().to_string(),
                stopped: false,
                metadata_removed: metadata_present,
                tmux_available: false,
                note: Some(
                    "`tmux` is not installed; removed local session metadata only".to_string(),
                ),
            }
        }
    };

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&output).context("serialize codex stop output")?
        );
    } else {
        println!("{}", render_stop_message(&output));
    }
    Ok(0)
}

fn run_top(all: bool, history: bool) -> Result<i32> {
    if !is_interactive_terminal() {
        bail!("`za codex top` requires a TTY");
    }

    let current_workspace_root = resolve_workspace_context()?.workspace_root;
    let mut listener = TopListenerHandle::start()?;
    let mut app = CodexTopApp::new(current_workspace_root, all, history);

    enable_raw_mode().context("enable raw terminal mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create ratatui terminal")?;

    let result = run_top_tui_loop(&mut terminal, &mut app, &mut listener);

    let mut teardown_err: Option<anyhow::Error> = None;
    if let Err(err) = disable_raw_mode().context("disable raw terminal mode") {
        teardown_err = Some(err);
    }
    if let Err(err) =
        execute!(terminal.backend_mut(), LeaveAlternateScreen).context("leave alternate screen")
    {
        teardown_err = Some(match teardown_err {
            Some(prev) => prev.context(format!("{err:#}")),
            None => err,
        });
    }
    if let Err(err) = terminal.show_cursor().context("restore cursor visibility") {
        teardown_err = Some(match teardown_err {
            Some(prev) => prev.context(format!("{err:#}")),
            None => err,
        });
    }
    if let Err(err) = listener.shutdown() {
        teardown_err = Some(match teardown_err {
            Some(prev) => prev.context(format!("{err:#}")),
            None => err,
        });
    }

    result?;
    if let Some(err) = teardown_err {
        return Err(err);
    }
    Ok(0)
}

fn run_top_tui_loop(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    app: &mut CodexTopApp,
    listener: &mut TopListenerHandle,
) -> Result<()> {
    loop {
        app.refresh(listener)?;
        terminal
            .draw(|frame| draw_top_tui(frame, app, listener))
            .context("draw codex top tui")?;

        if !event::poll(Duration::from_millis(120)).context("poll keyboard events")? {
            continue;
        }
        let Event::Key(key) = event::read().context("read keyboard event")? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        match key.code {
            KeyCode::Char('q') => return Ok(()),
            _ => app.handle_key(key.code),
        }
    }
}

#[derive(Clone, Copy)]
enum CodexLaunchMode {
    Fresh,
    ResumeLast,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TmuxProbe {
    Available,
    Missing,
}

#[derive(Debug)]
struct WorkspaceContext {
    workspace_root: PathBuf,
    workspace_label: String,
    workspace_hash: String,
    session_name: String,
    metadata_path: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SessionRecord {
    session_name: String,
    workspace_root: String,
    workspace_label: String,
    workspace_hash: String,
    created_at_unix: u64,
    launcher: String,
    launcher_args: Vec<String>,
}

#[derive(Clone, Debug)]
struct TmuxSessionInfo {
    created_unix: Option<u64>,
    activity_unix: Option<u64>,
    attached_clients: usize,
}

#[derive(Clone, Debug, Serialize)]
struct CodexSessionRow {
    session_name: String,
    status: String,
    attached_clients: usize,
    last_activity_unix: Option<u64>,
    created_unix: Option<u64>,
    codex_session_id: Option<String>,
    codex_model: Option<String>,
    codex_effort: Option<String>,
    context_left_percent: Option<f64>,
    workspace_root: Option<String>,
    workspace_label: Option<String>,
    metadata_present: bool,
}

#[derive(Debug, Serialize)]
struct CodexPsOutput {
    tmux_available: bool,
    sessions: Vec<CodexSessionRow>,
}

#[derive(Debug, Serialize)]
struct CodexStopOutput {
    session_name: String,
    workspace_root: String,
    stopped: bool,
    metadata_removed: bool,
    tmux_available: bool,
    note: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TopListenerState {
    endpoint: String,
    owner_pid: u32,
    updated_at_unix: u64,
}

#[derive(Clone, Debug)]
struct CodexSessionSummary {
    session_id: String,
    workspace_root: String,
    modified_unix: u64,
    model: Option<String>,
    effort: Option<String>,
    context_left_percent: Option<f64>,
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
struct TokenCountPayload {
    info: TokenCountInfo,
}

#[derive(Debug, Deserialize)]
struct TokenCountInfo {
    last_token_usage: TokenUsage,
    model_context_window: u64,
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
struct FileSessionState {
    session_id: Option<String>,
    workspace_root: Option<String>,
    started_unix: Option<u64>,
    model: Option<String>,
    effort: Option<String>,
    context_left_percent: Option<f64>,
    last_activity_unix: Option<u64>,
    last_event_name: Option<String>,
    event_count: u64,
    tool_calls: u64,
    tool_errors: u64,
}

#[derive(Clone, Debug)]
struct SessionFileTracker {
    path: PathBuf,
    offset: u64,
    modified_unix: u64,
    state: FileSessionState,
}

#[derive(Clone, Debug, Default)]
struct OtelSessionState {
    model: Option<String>,
    effort: Option<String>,
    workspace_root: Option<String>,
    last_activity_unix: Option<u64>,
    last_event_name: Option<String>,
    otel_events: u64,
    api_requests: u64,
    tool_calls: u64,
    tool_errors: u64,
    sse_events: u64,
}

#[derive(Clone, Debug, Default)]
struct OtelLiveState {
    sessions: BTreeMap<String, OtelSessionState>,
    session_events: BTreeMap<String, VecDeque<OtelEventRecord>>,
    total_events: u64,
    last_event_unix: Option<u64>,
}

#[derive(Clone, Debug)]
struct OtelEventRecord {
    observed_unix: u64,
    event_name: String,
    tool_error: bool,
    attributes: BTreeMap<String, String>,
    body: Option<String>,
}

#[derive(Clone, Debug)]
struct OtelSessionEvent {
    session_id: String,
    event_name: String,
    observed_unix: u64,
    model: Option<String>,
    effort: Option<String>,
    workspace_root: Option<String>,
    tool_error: bool,
    attributes: BTreeMap<String, String>,
    body: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OtlpLogsPayload {
    #[serde(rename = "resourceLogs", default)]
    resource_logs: Vec<OtlpResourceLogs>,
}

#[derive(Debug, Deserialize)]
struct OtlpResourceLogs {
    #[serde(rename = "scopeLogs", default)]
    scope_logs: Vec<OtlpScopeLogs>,
}

#[derive(Debug, Deserialize)]
struct OtlpScopeLogs {
    #[serde(rename = "logRecords", default)]
    log_records: Vec<OtlpLogRecord>,
}

#[derive(Debug, Deserialize)]
struct OtlpLogRecord {
    #[serde(rename = "observedTimeUnixNano")]
    observed_time_unix_nano: Option<String>,
    body: Option<serde_json::Value>,
    #[serde(default)]
    attributes: Vec<OtlpAttribute>,
}

#[derive(Debug, Deserialize)]
struct OtlpAttribute {
    key: String,
    value: serde_json::Value,
}

#[derive(Clone, Debug)]
struct CodexTopRow {
    key: String,
    session_id: Option<String>,
    managed_session_name: Option<String>,
    workspace_root: String,
    model: Option<String>,
    effort: Option<String>,
    context_left_percent: Option<f64>,
    status: String,
    tmux_running: bool,
    attached_clients: usize,
    last_activity_unix: Option<u64>,
    last_event_name: Option<String>,
    otel_events: u64,
    api_requests: u64,
    live_tool_calls: u64,
    lifetime_tool_calls: u64,
    live_tool_errors: u64,
    lifetime_tool_errors: u64,
    sse_events: u64,
    live_otel: bool,
}

#[derive(Debug)]
struct CodexTopApp {
    current_workspace_root: String,
    show_all: bool,
    show_history: bool,
    selected: usize,
    scroll_offset: usize,
    viewport_rows: usize,
    rows: Vec<CodexTopRow>,
    trackers: BTreeMap<PathBuf, SessionFileTracker>,
    otel_state: OtelLiveState,
    tmux_available: bool,
    tmux_sessions: BTreeMap<String, TmuxSessionInfo>,
    managed_records: Vec<SessionRecord>,
    last_refresh: Option<SystemTime>,
    last_discovery: Option<SystemTime>,
    status_message: Option<String>,
    view: TopView,
}

#[derive(Debug)]
enum TopView {
    Summary,
    Stream(TopStreamState),
}

#[derive(Debug)]
struct TopStreamState {
    session_id: String,
    workspace_root: String,
    model: Option<String>,
    effort: Option<String>,
    tmux_running: bool,
    live_otel: bool,
    selected: usize,
    scroll_offset: usize,
    viewport_rows: usize,
    follow: bool,
}

struct TopListenerHandle {
    endpoint: String,
    receiver: Receiver<OtelSessionEvent>,
    state_path: PathBuf,
    state: TopListenerState,
    stop: Arc<AtomicBool>,
    join_handle: Option<thread::JoinHandle<()>>,
    last_heartbeat: SystemTime,
}

struct TopRowsInput<'a> {
    current_workspace_root: &'a str,
    show_all: bool,
    show_history: bool,
    trackers: &'a BTreeMap<PathBuf, SessionFileTracker>,
    otel_state: &'a OtelLiveState,
    managed_records: &'a [SessionRecord],
    tmux_available: bool,
    tmux_sessions: &'a BTreeMap<String, TmuxSessionInfo>,
}

fn resolve_workspace_context() -> Result<WorkspaceContext> {
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

fn workspace_hash(root: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(root.to_string_lossy().as_bytes());
    format!("{:x}", hasher.finalize())
}

fn state_home() -> Result<PathBuf> {
    resolve_state_home(env_path("XDG_STATE_HOME"), env_path("HOME"))
}

fn resolve_state_home(xdg_state_home: Option<PathBuf>, home: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = xdg_state_home {
        return Ok(path);
    }
    let home = home
        .ok_or_else(|| anyhow!("cannot resolve state directory: set `XDG_STATE_HOME` or `HOME`"))?;
    Ok(home.join(".local/state"))
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn build_codex_launch_command(mode: CodexLaunchMode, extra_args: &[String]) -> Result<String> {
    let codex = crate::command::run::resolve_executable_path("codex")?;
    let listener = top_listener_state_for_launch(extra_args)?;
    let mut env_vars = crate::command::run::normalized_proxy_env_from_system()?;
    if listener.is_some() {
        ensure_local_listener_no_proxy(&mut env_vars);
    }
    let mut argv = Vec::new();
    argv.push(codex.display().to_string());
    argv.push("--no-alt-screen".to_string());
    argv.extend(top_listener_codex_args(listener.as_ref()));
    if matches!(mode, CodexLaunchMode::ResumeLast) {
        argv.push("resume".to_string());
        argv.push("--last".to_string());
    }
    argv.extend(extra_args.iter().cloned());
    build_shell_exec_command(&env_vars, &argv)
}

fn build_exec_command(args: &[String]) -> Result<String> {
    build_shell_exec_command(
        &crate::command::run::normalized_proxy_env_from_system()?,
        args,
    )
}

fn top_listener_state_for_launch(extra_args: &[String]) -> Result<Option<TopListenerState>> {
    if user_supplied_otel_config(extra_args) {
        return Ok(None);
    }

    load_active_top_listener_state()
}

fn top_listener_codex_args(listener: Option<&TopListenerState>) -> Vec<String> {
    let Some(listener) = listener else {
        return Vec::new();
    };
    vec![
        "-c".to_string(),
        format!(
            "otel.exporter={{otlp-http={{endpoint=\"{}\",protocol=\"json\"}}}}",
            listener.endpoint
        ),
        "-c".to_string(),
        "otel.log_user_prompt=false".to_string(),
    ]
}

fn ensure_local_listener_no_proxy(env_vars: &mut Vec<(String, String)>) {
    const LOCAL_RULES: [&str; 3] = ["127.0.0.1", "localhost", "::1"];

    let mut rules = env_vars
        .iter()
        .find_map(|(key, value)| {
            (key == "NO_PROXY" || key == "no_proxy").then_some(parse_no_proxy_rules(value))
        })
        .unwrap_or_default();
    for rule in LOCAL_RULES {
        if !rules.iter().any(|existing| existing == rule) {
            rules.push(rule.to_string());
        }
    }
    let value = rules.join(",");
    let mut saw_upper = false;
    let mut saw_lower = false;
    for (key, current) in env_vars.iter_mut() {
        if key == "NO_PROXY" {
            *current = value.clone();
            saw_upper = true;
        } else if key == "no_proxy" {
            *current = value.clone();
            saw_lower = true;
        }
    }
    if !saw_upper {
        env_vars.push(("NO_PROXY".to_string(), value.clone()));
    }
    if !saw_lower {
        env_vars.push(("no_proxy".to_string(), value));
    }
}

fn parse_no_proxy_rules(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|rule| !rule.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn user_supplied_otel_config(args: &[String]) -> bool {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "-c" || arg == "--config" {
            if let Some(value) = args.get(index + 1)
                && config_overrides_otel(value)
            {
                return true;
            }
            index += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--config=")
            && config_overrides_otel(value)
        {
            return true;
        }
        index += 1;
    }
    false
}

fn config_overrides_otel(value: &str) -> bool {
    value
        .split_once('=')
        .map(|(key, _)| key.trim())
        .is_some_and(|key| key == "otel" || key.starts_with("otel."))
}

fn build_shell_exec_command(env_vars: &[(String, String)], argv: &[String]) -> Result<String> {
    if argv.is_empty() {
        bail!("cannot build empty shell command");
    }

    let mut parts = Vec::with_capacity(env_vars.len() * 2 + argv.len() + 2);
    parts.push("exec".to_string());
    if !env_vars.is_empty() {
        parts.push("env".to_string());
        for (key, value) in env_vars {
            parts.push(format!("{key}={}", shell_escape(value)));
        }
    }
    parts.extend(argv.iter().map(|arg| shell_escape(arg)));
    Ok(parts.join(" "))
}

fn shell_escape(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }

    let mut out = String::from("'");
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\"'\"'");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

fn persist_session_record(
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

fn top_listener_state_path() -> Result<PathBuf> {
    Ok(state_home()?.join(TOP_LISTENER_STATE_RELATIVE))
}

fn load_active_top_listener_state() -> Result<Option<TopListenerState>> {
    let path = top_listener_state_path()?;
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };

    let state = match serde_json::from_slice::<TopListenerState>(&bytes) {
        Ok(state) => state,
        Err(_) => {
            let _ = fs::remove_file(&path);
            return Ok(None);
        }
    };
    let now = current_unix_seconds();
    if now.saturating_sub(state.updated_at_unix) > TOP_LISTENER_STALE_SECS {
        let _ = fs::remove_file(&path);
        return Ok(None);
    }
    Ok(Some(state))
}

fn write_top_listener_state(path: &Path, state: &TopListenerState) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create listener state directory {}", parent.display()))?;
    }
    fs::write(
        path,
        serde_json::to_vec_pretty(state).context("serialize codex top listener state")?,
    )
    .with_context(|| format!("write {}", path.display()))
}

fn remove_top_listener_state(path: &Path, endpoint: &str) -> Result<()> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };
    let should_remove = serde_json::from_slice::<TopListenerState>(&bytes)
        .ok()
        .is_none_or(|state| state.endpoint == endpoint);
    if should_remove {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => return Err(err).with_context(|| format!("remove {}", path.display())),
        }
    }
    Ok(())
}

fn remove_session_record(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("remove session metadata {}", path.display())),
    }
}

fn ensure_tmux_available() -> Result<()> {
    match probe_tmux()? {
        TmuxProbe::Available => Ok(()),
        TmuxProbe::Missing => bail!("`za codex` requires `tmux`; install it first"),
    }
}

fn probe_tmux() -> Result<TmuxProbe> {
    match Command::new("tmux").arg("-V").output() {
        Ok(output) if output.status.success() => Ok(TmuxProbe::Available),
        Ok(output) => bail!(
            "`za codex` requires a working `tmux`; `tmux -V` exited with status {}",
            output.status
        ),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(TmuxProbe::Missing),
        Err(err) => Err(err).context("run `tmux -V`"),
    }
}

fn tmux_has_session(session_name: &str) -> Result<bool> {
    let output = Command::new("tmux")
        .args(["has-session", "-t", session_name])
        .output()
        .with_context(|| format!("check tmux session `{session_name}`"))?;
    if output.status.success() {
        return Ok(true);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if is_tmux_session_absent(&stderr) {
        return Ok(false);
    }
    bail!(
        "`tmux has-session -t {session_name}` failed: {}",
        stderr.trim()
    )
}

fn tmux_session_needs_top_listener_restart(
    session_name: &str,
    listener: Option<&TopListenerState>,
) -> Result<bool> {
    let Some(listener) = listener else {
        return Ok(false);
    };
    Ok(!tmux_panes_include_listener_endpoint(
        &tmux_list_panes_start_commands(session_name)?,
        &listener.endpoint,
    ))
}

fn tmux_list_panes_start_commands(session_name: &str) -> Result<String> {
    let output = Command::new("tmux")
        .args([
            "list-panes",
            "-t",
            session_name,
            "-F",
            "#{pane_current_command}\t#{pane_start_command}",
        ])
        .output()
        .with_context(|| format!("list tmux panes for `{session_name}`"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if is_tmux_session_absent(&stderr) {
            return Ok(String::new());
        }
        bail!(
            "`tmux list-panes -t {session_name}` failed: {}",
            stderr.trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn tmux_disable_alternate_screen_for_codex_windows(session_name: &str) -> Result<()> {
    for window_id in tmux_codex_window_ids(session_name)? {
        tmux_set_window_option(&window_id, "alternate-screen", "off")?;
    }
    Ok(())
}

fn tmux_ensure_outer_scrollback_preserved() -> Result<()> {
    if tmux_terminal_overrides_disable_alt_screen(&tmux_show_server_option("terminal-overrides")?) {
        return Ok(());
    }
    let output = Command::new("tmux")
        .args([
            "set-option",
            "-sa",
            "terminal-overrides",
            ",*:smcup@:rmcup@",
        ])
        .output()
        .context("append tmux terminal-overrides to preserve outer scrollback")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "`tmux set-option -sa terminal-overrides ',*:smcup@:rmcup@'` failed: {}",
            stderr.trim()
        );
    }
    Ok(())
}

fn tmux_show_server_option(option: &str) -> Result<String> {
    let output = Command::new("tmux")
        .args(["show-options", "-s", option])
        .output()
        .with_context(|| format!("show tmux server option `{option}`"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.trim().eq_ignore_ascii_case("invalid option") {
            return Ok(String::new());
        }
        bail!("`tmux show-options -s {option}` failed: {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn tmux_codex_window_ids(session_name: &str) -> Result<BTreeSet<String>> {
    let output = Command::new("tmux")
        .args([
            "list-panes",
            "-t",
            session_name,
            "-F",
            "#{pane_current_command}\t#{window_id}",
        ])
        .output()
        .with_context(|| format!("list tmux codex panes for `{session_name}`"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if is_tmux_session_absent(&stderr) {
            return Ok(BTreeSet::new());
        }
        bail!(
            "`tmux list-panes -t {session_name}` failed: {}",
            stderr.trim()
        );
    }
    Ok(parse_tmux_codex_window_ids(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

fn parse_tmux_codex_window_ids(output: &str) -> BTreeSet<String> {
    output
        .lines()
        .filter_map(|line| {
            let (command, window_id) = line.split_once('\t')?;
            (command.trim() == "codex")
                .then_some(window_id.trim())
                .filter(|window_id| !window_id.is_empty())
                .map(ToOwned::to_owned)
        })
        .collect()
}

fn tmux_terminal_overrides_disable_alt_screen(output: &str) -> bool {
    output.lines().any(|line| {
        let trimmed = line.trim();
        trimmed.contains("smcup@") && trimmed.contains("rmcup@")
    })
}

fn tmux_set_window_option(target: &str, option: &str, value: &str) -> Result<()> {
    let output = Command::new("tmux")
        .args(["set-window-option", "-t", target, option, value])
        .output()
        .with_context(|| format!("set tmux window option `{option}` for `{target}`"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "`tmux set-window-option -t {target} {option} {value}` failed: {}",
            stderr.trim()
        );
    }
    Ok(())
}

fn tmux_panes_include_listener_endpoint(output: &str, endpoint: &str) -> bool {
    output.lines().any(|line| {
        let Some((current_command, start_command)) = line.split_once('\t') else {
            return false;
        };
        current_command.trim() == "codex" && start_command.contains(endpoint)
    })
}

fn tmux_new_session(session_name: &str, cwd: &Path, command: &str) -> Result<()> {
    let output = Command::new("tmux")
        .arg("new-session")
        .arg("-d")
        .arg("-s")
        .arg(session_name)
        .arg("-c")
        .arg(cwd)
        .arg(command)
        .output()
        .with_context(|| format!("create tmux session `{session_name}`"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "`tmux new-session -s {session_name}` failed: {}",
            stderr.trim()
        );
    }
    Ok(())
}

fn tmux_new_window(
    session_name: &str,
    window_name: &str,
    cwd: &Path,
    command: &str,
    detached: bool,
) -> Result<()> {
    let mut cmd = Command::new("tmux");
    cmd.arg("new-window")
        .arg("-t")
        .arg(session_name)
        .arg("-n")
        .arg(window_name)
        .arg("-c")
        .arg(cwd);
    if detached {
        cmd.arg("-d");
    }
    let output = cmd
        .arg(command)
        .output()
        .with_context(|| format!("create tmux window `{window_name}` in `{session_name}`"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "`tmux new-window -t {session_name}` failed: {}",
            stderr.trim()
        );
    }
    Ok(())
}

fn tmux_kill_session(session_name: &str) -> Result<()> {
    let output = Command::new("tmux")
        .args(["kill-session", "-t", session_name])
        .output()
        .with_context(|| format!("stop tmux session `{session_name}`"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if is_tmux_session_absent(&stderr) {
            return Ok(());
        }
        bail!(
            "`tmux kill-session -t {session_name}` failed: {}",
            stderr.trim()
        );
    }
    Ok(())
}

fn maybe_attach_or_report(session_name: &str, workspace_root: &Path) -> Result<i32> {
    if is_interactive_terminal() {
        return attach_session(session_name);
    }

    println!(
        "Codex session `{}` is ready for {}.",
        session_name,
        workspace_root.display()
    );
    Ok(0)
}

fn attach_session(session_name: &str) -> Result<i32> {
    tmux_ensure_outer_scrollback_preserved()?;
    tmux_disable_alternate_screen_for_codex_windows(session_name)?;
    let mut cmd = Command::new("tmux");
    if env::var_os("TMUX").is_some() {
        cmd.args(["switch-client", "-t", session_name]);
    } else {
        cmd.args(["attach-session", "-d", "-t", session_name]);
    }

    let status = cmd
        .status()
        .with_context(|| format!("attach tmux session `{session_name}`"))?;
    Ok(status.code().unwrap_or(130))
}

fn is_interactive_terminal() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

fn collect_session_rows(
    tmux_sessions: &BTreeMap<String, TmuxSessionInfo>,
    tmux_available: bool,
) -> Result<Vec<CodexSessionRow>> {
    let mut rows = Vec::new();
    let mut seen = BTreeSet::new();
    let records = load_session_records()?;
    let codex_summaries = load_codex_session_summaries(&records)?;
    let missing_context_session_ids = codex_summaries
        .values()
        .filter(|summary| summary.context_left_percent.is_none())
        .map(|summary| summary.session_id.clone())
        .collect::<BTreeSet<_>>();
    let legacy_codex_context =
        load_legacy_codex_context_left_percent_by_session_id(&missing_context_session_ids)?;

    for record in records {
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

fn load_session_records() -> Result<Vec<SessionRecord>> {
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

fn load_codex_session_summaries(
    records: &[SessionRecord],
) -> Result<BTreeMap<String, CodexSessionSummary>> {
    let workspace_starts = records
        .iter()
        .map(|record| (record.workspace_root.clone(), record.created_at_unix))
        .collect::<BTreeMap<_, _>>();
    if workspace_starts.is_empty() {
        return Ok(BTreeMap::new());
    }

    let sessions_root = codex_home()?.join("sessions");
    if !sessions_root.exists() {
        return Ok(BTreeMap::new());
    }

    let mut best: BTreeMap<String, CodexSessionSummary> = BTreeMap::new();
    for dent in WalkBuilder::new(&sessions_root)
        .standard_filters(false)
        .hidden(false)
        .build()
    {
        let dent = dent.with_context(|| format!("walk {}", sessions_root.display()))?;
        let path = dent.path();
        if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }

        let Some(summary) = summarize_codex_session_file(path, &workspace_starts)? else {
            continue;
        };
        let workspace_root = summary.workspace_root.clone();
        match best.get(&workspace_root) {
            Some(current) if current.modified_unix >= summary.modified_unix => {}
            _ => {
                best.insert(workspace_root, summary);
            }
        }
    }

    Ok(best)
}

fn load_legacy_codex_context_left_percent_by_session_id(
    session_ids: &BTreeSet<String>,
) -> Result<BTreeMap<String, f64>> {
    if session_ids.is_empty() {
        return Ok(BTreeMap::new());
    }

    let log_path = codex_home()?.join("log/codex-tui.log");
    let file = match fs::File::open(&log_path) {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(err) => return Err(err).with_context(|| format!("open {}", log_path.display())),
    };
    parse_legacy_codex_context_left_percent_lines(BufReader::new(file), session_ids)
}

fn codex_home() -> Result<PathBuf> {
    if let Some(path) = env_path("CODEX_HOME") {
        return Ok(path);
    }
    let home = env_path("HOME")
        .ok_or_else(|| anyhow!("cannot resolve Codex home: set `CODEX_HOME` or `HOME`"))?;
    Ok(home.join(".codex"))
}

impl TopListenerHandle {
    fn start() -> Result<Self> {
        if let Some(active) = load_active_top_listener_state()? {
            bail!(
                "another `za codex top` is already running at {}; stop it or wait for its listener state to expire",
                active.endpoint
            );
        }

        let listener =
            TcpListener::bind(("127.0.0.1", 0)).context("bind local Codex OTLP listener")?;
        listener
            .set_nonblocking(true)
            .context("configure Codex OTLP listener socket")?;
        let port = listener
            .local_addr()
            .context("read Codex OTLP listener address")?
            .port();
        let endpoint = format!("http://127.0.0.1:{port}/v1/logs");
        let state_path = top_listener_state_path()?;
        let state = TopListenerState {
            endpoint: endpoint.clone(),
            owner_pid: process::id(),
            updated_at_unix: current_unix_seconds(),
        };
        write_top_listener_state(&state_path, &state)?;

        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let (sender, receiver) = mpsc::channel();
        let join_handle = thread::spawn(move || {
            while !stop_flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = handle_otel_stream(stream, &sender);
                    }
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(100));
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            endpoint,
            receiver,
            state_path,
            state,
            stop,
            join_handle: Some(join_handle),
            last_heartbeat: SystemTime::now(),
        })
    }

    fn heartbeat(&mut self) -> Result<()> {
        let now = SystemTime::now();
        let elapsed = now.duration_since(self.last_heartbeat).unwrap_or_default();
        if elapsed < TOP_LISTENER_HEARTBEAT_INTERVAL {
            return Ok(());
        }
        self.state.updated_at_unix = current_unix_seconds();
        write_top_listener_state(&self.state_path, &self.state)?;
        self.last_heartbeat = now;
        Ok(())
    }

    fn shutdown(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(authority) = self
            .endpoint
            .strip_prefix("http://")
            .and_then(|value| value.split('/').next())
        {
            let _ = TcpStream::connect(authority);
        }
        if let Some(handle) = self.join_handle.take() {
            handle
                .join()
                .map_err(|_| anyhow!("join Codex OTLP listener thread"))?;
        }
        remove_top_listener_state(&self.state_path, &self.endpoint)
    }
}

impl Drop for TopListenerHandle {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn handle_otel_stream(stream: TcpStream, sender: &Sender<OtelSessionEvent>) -> Result<()> {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .context("set OTLP read timeout")?;
    let mut writer = stream
        .try_clone()
        .context("clone OTLP stream for response write")?;
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    if reader
        .read_line(&mut request_line)
        .context("read OTLP request line")?
        == 0
    {
        return Ok(());
    }

    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        let bytes = reader
            .read_line(&mut line)
            .context("read OTLP header line")?;
        if bytes == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            content_length = value
                .trim()
                .parse()
                .context("parse OTLP content-length header")?;
        }
    }

    let mut body = vec![0; content_length];
    reader
        .read_exact(&mut body)
        .context("read OTLP request body")?;
    for event in parse_otlp_session_events(&body)? {
        let _ = sender.send(event);
    }

    writer
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
        .context("write OTLP response")?;
    writer.flush().context("flush OTLP response")
}

fn parse_otlp_session_events(body: &[u8]) -> Result<Vec<OtelSessionEvent>> {
    let payload =
        serde_json::from_slice::<OtlpLogsPayload>(body).context("parse OTLP JSON body")?;
    let mut events = Vec::new();
    for resource in payload.resource_logs {
        for scope in resource.scope_logs {
            for record in scope.log_records {
                let attributes = otlp_attributes_map(&record.attributes);
                let Some(session_id) = attributes.get("conversation.id").cloned() else {
                    continue;
                };
                let Some(event_name) = attributes.get("event.name").cloned() else {
                    continue;
                };
                let observed_unix =
                    parse_observed_unix_secs(record.observed_time_unix_nano.as_deref())
                        .unwrap_or_else(current_unix_seconds);
                events.push(OtelSessionEvent {
                    session_id,
                    event_name,
                    observed_unix,
                    model: attributes
                        .get("model")
                        .cloned()
                        .or_else(|| attributes.get("slug").cloned()),
                    effort: attributes.get("reasoning_effort").cloned(),
                    workspace_root: attributes
                        .get("cwd")
                        .cloned()
                        .or_else(|| attributes.get("workspace_root").cloned())
                        .or_else(|| attributes.get("workspace").cloned()),
                    tool_error: otlp_event_has_error(&record.attributes),
                    attributes,
                    body: record.body.as_ref().and_then(otlp_value_string),
                });
            }
        }
    }
    Ok(events)
}

fn parse_observed_unix_secs(value: Option<&str>) -> Option<u64> {
    let nanos = value?.trim().parse::<u128>().ok()?;
    Some((nanos / 1_000_000_000) as u64)
}

fn otlp_attr_string(attributes: &[OtlpAttribute], key: &str) -> Option<String> {
    attributes
        .iter()
        .find(|attribute| attribute.key == key)
        .and_then(|attribute| otlp_value_string(&attribute.value))
}

fn otlp_attr_bool(attributes: &[OtlpAttribute], key: &str) -> Option<bool> {
    let value = attributes.iter().find(|attribute| attribute.key == key)?;
    let object = value.value.as_object()?;
    object.get("boolValue").and_then(serde_json::Value::as_bool)
}

fn otlp_attributes_map(attributes: &[OtlpAttribute]) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for attribute in attributes {
        if let Some(value) = otlp_value_string(&attribute.value) {
            map.insert(attribute.key.clone(), value);
        }
    }
    map
}

fn otlp_value_string(value: &serde_json::Value) -> Option<String> {
    let object = value.as_object()?;
    if let Some(value) = object
        .get("stringValue")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(value.to_string());
    }
    if let Some(value) = object
        .get("intValue")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(value.to_string());
    }
    if let Some(value) = object.get("boolValue").and_then(serde_json::Value::as_bool) {
        return Some(value.to_string());
    }
    object
        .get("doubleValue")
        .and_then(serde_json::Value::as_f64)
        .map(|value| format!("{value}"))
}

fn otlp_event_has_error(attributes: &[OtlpAttribute]) -> bool {
    const ERROR_KEYS: [&str; 5] = [
        "error",
        "error.message",
        "tool.error",
        "tool_error",
        "exception.message",
    ];
    const SUCCESS_KEYS: [&str; 4] = ["success", "tool.success", "ok", "tool.ok"];

    if SUCCESS_KEYS
        .iter()
        .any(|key| otlp_attr_bool(attributes, key) == Some(false))
    {
        return true;
    }

    ERROR_KEYS.iter().any(|key| {
        otlp_attr_string(attributes, key)
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
    })
}

impl SessionFileTracker {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            offset: 0,
            modified_unix: 0,
            state: FileSessionState::default(),
        }
    }

    fn sync(&mut self) -> Result<()> {
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

    fn key(&self) -> String {
        self.state
            .session_id
            .clone()
            .unwrap_or_else(|| format!("file:{}", self.path.display()))
    }
}

impl CodexTopApp {
    fn new(current_workspace_root: PathBuf, show_all: bool, show_history: bool) -> Self {
        Self {
            current_workspace_root: current_workspace_root.display().to_string(),
            show_all,
            show_history,
            selected: 0,
            scroll_offset: 0,
            viewport_rows: 10,
            rows: Vec::new(),
            trackers: BTreeMap::new(),
            otel_state: OtelLiveState::default(),
            tmux_available: false,
            tmux_sessions: BTreeMap::new(),
            managed_records: Vec::new(),
            last_refresh: None,
            last_discovery: None,
            status_message: None,
            view: TopView::Summary,
        }
    }

    fn refresh(&mut self, listener: &mut TopListenerHandle) -> Result<()> {
        listener.heartbeat()?;
        let drained = self.drain_otel(listener);
        let now = SystemTime::now();
        let should_full_refresh = self
            .last_refresh
            .and_then(|last| now.duration_since(last).ok())
            .is_none_or(|elapsed| elapsed >= TOP_REFRESH_INTERVAL);
        if should_full_refresh {
            self.refresh_trackers(now)?;
            self.managed_records = load_session_records()?;
            match probe_tmux()? {
                TmuxProbe::Available => {
                    self.tmux_available = true;
                    self.tmux_sessions = list_tmux_sessions()?;
                }
                TmuxProbe::Missing => {
                    self.tmux_available = false;
                    self.tmux_sessions.clear();
                }
            }
            self.last_refresh = Some(now);
        }

        if drained || should_full_refresh || self.rows.is_empty() {
            let selected_key = self.rows.get(self.selected).map(|row| row.key.clone());
            self.rows = build_top_rows(TopRowsInput {
                current_workspace_root: &self.current_workspace_root,
                show_all: self.show_all,
                show_history: self.show_history,
                trackers: &self.trackers,
                otel_state: &self.otel_state,
                managed_records: &self.managed_records,
                tmux_available: self.tmux_available,
                tmux_sessions: &self.tmux_sessions,
            });
            if let Some(selected_key) = selected_key {
                if let Some(index) = self.rows.iter().position(|row| row.key == selected_key) {
                    self.selected = index;
                } else if self.selected >= self.rows.len() {
                    self.selected = self.rows.len().saturating_sub(1);
                }
            } else if self.selected >= self.rows.len() {
                self.selected = self.rows.len().saturating_sub(1);
            }
            if self.rows.is_empty() {
                self.scroll_offset = 0;
            }
        }
        self.rebind_stream_session_if_needed();
        Ok(())
    }

    fn refresh_trackers(&mut self, now: SystemTime) -> Result<()> {
        let should_discover = self
            .last_discovery
            .and_then(|last| now.duration_since(last).ok())
            .is_none_or(|elapsed| elapsed >= TOP_DISCOVERY_INTERVAL);
        if should_discover {
            let paths = discover_codex_session_paths()?;
            let wanted = paths.iter().cloned().collect::<BTreeSet<_>>();
            for path in paths {
                self.trackers
                    .entry(path.clone())
                    .or_insert_with(|| SessionFileTracker::new(path));
            }
            self.trackers.retain(|path, _| wanted.contains(path));
            self.last_discovery = Some(now);
        }

        for tracker in self.trackers.values_mut() {
            tracker.sync()?;
        }
        Ok(())
    }

    fn drain_otel(&mut self, listener: &mut TopListenerHandle) -> bool {
        let mut changed = false;
        loop {
            match listener.receiver.try_recv() {
                Ok(event) => {
                    self.apply_otel_event(event);
                    changed = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.status_message = Some("live OTLP listener disconnected".to_string());
                    break;
                }
            }
        }
        changed
    }

    fn apply_otel_event(&mut self, event: OtelSessionEvent) {
        let session = self
            .otel_state
            .sessions
            .entry(event.session_id.clone())
            .or_default();
        if let Some(model) = event.model.filter(|value| !value.is_empty()) {
            session.model = Some(model);
        }
        if let Some(effort) = event.effort.filter(|value| !value.is_empty()) {
            session.effort = Some(effort);
        }
        if let Some(workspace_root) = event.workspace_root.filter(|value| !value.is_empty()) {
            session.workspace_root = Some(workspace_root);
        }
        session.last_activity_unix = Some(
            session
                .last_activity_unix
                .unwrap_or_default()
                .max(event.observed_unix),
        );
        session.last_event_name = Some(event.event_name.clone());
        session.otel_events += 1;
        if event.event_name.ends_with("api_request") {
            session.api_requests += 1;
        }
        if event.event_name.ends_with("sse_event") {
            session.sse_events += 1;
        }
        if event.event_name.ends_with("tool_result")
            || event.event_name.ends_with("tool_call")
            || event.event_name.contains(".tool_")
        {
            session.tool_calls += 1;
            if event.tool_error {
                session.tool_errors += 1;
            }
        }
        self.otel_state.total_events += 1;
        self.otel_state.last_event_unix = Some(
            self.otel_state
                .last_event_unix
                .unwrap_or_default()
                .max(event.observed_unix),
        );

        let session_events = self
            .otel_state
            .session_events
            .entry(event.session_id.clone())
            .or_default();
        session_events.push_back(OtelEventRecord {
            observed_unix: event.observed_unix,
            event_name: event.event_name.clone(),
            tool_error: event.tool_error,
            attributes: event.attributes,
            body: event.body,
        });
        while session_events.len() > TOP_STREAM_EVENT_CAP {
            session_events.pop_front();
        }

        if let TopView::Stream(stream) = &mut self.view
            && stream.follow
            && stream.session_id == event.session_id
        {
            stream.selected = session_events.len().saturating_sub(1);
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        if delta.is_negative() {
            self.selected = self.selected.saturating_sub(delta.unsigned_abs());
        } else {
            self.selected = self
                .selected
                .saturating_add(delta as usize)
                .min(self.rows.len().saturating_sub(1));
        }
    }

    fn move_to_start(&mut self) {
        self.selected = 0;
    }

    fn move_to_end(&mut self) {
        self.selected = self.rows.len().saturating_sub(1);
    }

    fn page_down(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let step = self.viewport_rows.saturating_sub(1).max(1);
        self.selected = self
            .selected
            .saturating_add(step)
            .min(self.rows.len().saturating_sub(1));
    }

    fn page_up(&mut self) {
        let step = self.viewport_rows.saturating_sub(1).max(1);
        self.selected = self.selected.saturating_sub(step);
    }

    fn toggle_scope(&mut self) {
        self.show_all = !self.show_all;
        self.selected = 0;
        self.scroll_offset = 0;
        self.status_message = Some(if self.show_all {
            "scope switched to all local Codex sessions".to_string()
        } else {
            "scope switched to current workspace".to_string()
        });
    }

    fn toggle_history(&mut self) {
        self.show_history = !self.show_history;
        self.selected = 0;
        self.scroll_offset = 0;
        self.status_message = Some(if self.show_history {
            "history rows enabled".to_string()
        } else {
            "history rows hidden; showing active sessions only".to_string()
        });
    }

    fn handle_key(&mut self, code: KeyCode) {
        if matches!(self.view, TopView::Summary) {
            self.handle_summary_key(code);
        } else {
            self.handle_stream_key(code);
        }
    }

    fn handle_summary_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {}
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Home | KeyCode::Char('g') => self.move_to_start(),
            KeyCode::End | KeyCode::Char('G') => self.move_to_end(),
            KeyCode::PageDown => self.page_down(),
            KeyCode::PageUp => self.page_up(),
            KeyCode::Char('a') => self.toggle_scope(),
            KeyCode::Char('h') => self.toggle_history(),
            KeyCode::Enter => self.open_selected_stream(),
            _ => {}
        }
    }

    fn handle_stream_key(&mut self, code: KeyCode) {
        let (session_id, viewport_rows, selected, follow) = match &self.view {
            TopView::Stream(stream) => (
                stream.session_id.clone(),
                stream.viewport_rows,
                stream.selected,
                stream.follow,
            ),
            TopView::Summary => return,
        };
        let event_len = self.stream_event_len(&session_id);

        match code {
            KeyCode::Esc | KeyCode::Backspace => self.view = TopView::Summary,
            KeyCode::Down | KeyCode::Char('j') => {
                self.update_stream_state(|stream| {
                    stream.follow = false;
                    stream.selected = selected.saturating_add(1).min(event_len.saturating_sub(1));
                });
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.update_stream_state(|stream| {
                    stream.follow = false;
                    stream.selected = selected.saturating_sub(1);
                });
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.update_stream_state(|stream| {
                    stream.follow = false;
                    stream.selected = 0;
                });
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.update_stream_state(|stream| {
                    stream.follow = true;
                    stream.selected = event_len.saturating_sub(1);
                });
            }
            KeyCode::PageDown => {
                let step = viewport_rows.saturating_sub(1).max(1);
                self.update_stream_state(|stream| {
                    stream.follow = false;
                    stream.selected = selected
                        .saturating_add(step)
                        .min(event_len.saturating_sub(1));
                });
            }
            KeyCode::PageUp => {
                let step = viewport_rows.saturating_sub(1).max(1);
                self.update_stream_state(|stream| {
                    stream.follow = false;
                    stream.selected = selected.saturating_sub(step);
                });
            }
            KeyCode::Char('f') => {
                let next_follow = !follow;
                self.update_stream_state(|stream| {
                    stream.follow = next_follow;
                    if next_follow {
                        stream.selected = event_len.saturating_sub(1);
                    }
                });
                self.status_message = Some(if next_follow {
                    "stream follow enabled".to_string()
                } else {
                    "stream follow paused".to_string()
                });
            }
            _ => {}
        }
    }

    fn open_selected_stream(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        let Some(session_id) = row.session_id.clone() else {
            self.status_message = Some(
                "selected row has no Codex conversation id; cannot open OTel stream".to_string(),
            );
            return;
        };
        let selected = self.stream_event_len(&session_id).saturating_sub(1);
        self.view = TopView::Stream(TopStreamState {
            session_id,
            workspace_root: row.workspace_root.clone(),
            model: row.model.clone(),
            effort: row.effort.clone(),
            tmux_running: row.tmux_running,
            live_otel: row.live_otel,
            selected,
            scroll_offset: 0,
            viewport_rows: 10,
            follow: true,
        });
    }

    fn rebind_stream_session_if_needed(&mut self) {
        let Some((next_session_id, next_model, next_effort, next_tmux_running, next_live_otel)) =
            (match &self.view {
                TopView::Summary => None,
                TopView::Stream(stream) => preferred_stream_row(
                    &self.rows,
                    &stream.session_id,
                    &stream.workspace_root,
                    &self.otel_state,
                )
                .and_then(|row| {
                    row.session_id.as_ref().map(|session_id| {
                        (
                            session_id.clone(),
                            row.model.clone(),
                            row.effort.clone(),
                            row.tmux_running,
                            row.live_otel,
                        )
                    })
                }),
            })
        else {
            return;
        };
        let next_event_len = self.stream_event_len(&next_session_id);
        self.update_stream_state(|stream| {
            if stream.session_id == next_session_id {
                return;
            }
            stream.session_id = next_session_id.clone();
            stream.model = next_model.clone();
            stream.effort = next_effort.clone();
            stream.tmux_running = next_tmux_running;
            stream.live_otel = next_live_otel;
            stream.selected = if stream.follow {
                next_event_len.saturating_sub(1)
            } else {
                stream.selected.min(next_event_len.saturating_sub(1))
            };
            stream.scroll_offset = 0;
        });
        self.status_message = Some(format!(
            "stream rebound to live OTel session {}",
            truncate_end(&next_session_id, 12)
        ));
    }

    fn stream_event_len(&self, session_id: &str) -> usize {
        self.stream_event_vec(session_id).len()
    }

    fn update_stream_state(&mut self, update: impl FnOnce(&mut TopStreamState)) {
        if let TopView::Stream(stream) = &mut self.view {
            update(stream);
        }
    }

    fn stream_event_vec(&self, session_id: &str) -> Vec<OtelEventRecord> {
        self.otel_state
            .session_events
            .get(session_id)
            .map(|events| events.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default()
    }
}

fn discover_codex_session_paths() -> Result<Vec<PathBuf>> {
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
    Ok(paths)
}

fn apply_session_log_line(
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
                    state.tool_calls += 1;
                    if payload.get("status").and_then(serde_json::Value::as_str) == Some("failed") {
                        state.tool_errors += 1;
                    }
                }
                "custom_tool_call_output" => {
                    if custom_tool_output_failed(&payload) {
                        state.tool_errors += 1;
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
        state.event_count += 1;
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

fn build_top_rows(input: TopRowsInput<'_>) -> Vec<CodexTopRow> {
    let TopRowsInput {
        current_workspace_root,
        show_all,
        show_history,
        trackers,
        otel_state,
        managed_records,
        tmux_available,
        tmux_sessions,
    } = input;
    let mut rows = Vec::new();
    let mut seen_keys = BTreeSet::new();
    let mut managed_assignments = BTreeMap::new();
    let mut synthetic_records = Vec::new();
    let visible_trackers = trackers
        .values()
        .filter(|tracker| {
            workspace_visible(
                show_all,
                current_workspace_root,
                tracker.state.workspace_root.as_deref(),
            )
        })
        .collect::<Vec<_>>();
    let mut assigned_tracker_keys = BTreeSet::new();

    let mut visible_records = managed_records
        .iter()
        .filter(|record| {
            workspace_visible(
                show_all,
                current_workspace_root,
                Some(&record.workspace_root),
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    visible_records.sort_by_key(|record| record.created_at_unix);

    for record in visible_records {
        if let Some(tracker_key) =
            best_tracker_match_for_record(&record, &visible_trackers, &assigned_tracker_keys)
        {
            assigned_tracker_keys.insert(tracker_key.clone());
            managed_assignments.insert(tracker_key, record.clone());
        } else {
            synthetic_records.push(record.clone());
        }
    }

    for tracker in trackers.values() {
        let workspace_root = tracker.state.workspace_root.clone().or_else(|| {
            tracker
                .state
                .session_id
                .as_ref()
                .and_then(|id| otel_state.sessions.get(id))
                .and_then(|session| session.workspace_root.clone())
        });
        if !workspace_visible(show_all, current_workspace_root, workspace_root.as_deref()) {
            continue;
        }

        let key = tracker.key();
        let tracker_otel = tracker.state.session_id.as_ref().and_then(|session_id| {
            otel_state
                .sessions
                .get(session_id)
                .map(|session| (session_id, session))
        });
        let workspace_otel = workspace_root
            .as_deref()
            .and_then(|workspace_root| latest_workspace_otel_session(otel_state, workspace_root));
        let otel = pick_preferred_row_otel(tracker_otel, workspace_otel);
        let managed_record = managed_assignments.get(&key);
        let tmux = managed_record.and_then(|record| tmux_sessions.get(&record.session_name));
        let tmux_running = tmux.is_some();
        let live_otel = otel
            .and_then(|(_, session)| session.last_activity_unix)
            .is_some_and(|last| current_unix_seconds().saturating_sub(last) <= 5);
        let last_activity_unix = select_latest_activity(
            tracker
                .state
                .last_activity_unix
                .or(Some(tracker.modified_unix)),
            otel.and_then(|(_, session)| session.last_activity_unix),
        );
        let status = top_row_status(
            tmux_running,
            managed_record.is_some(),
            tmux_available,
            live_otel,
            last_activity_unix,
        )
        .to_string();

        let row_session_id = otel
            .map(|(session_id, _)| session_id.clone())
            .or_else(|| tracker.state.session_id.clone());
        rows.push(CodexTopRow {
            key: key.clone(),
            session_id: row_session_id.clone(),
            managed_session_name: managed_record.map(|record| record.session_name.clone()),
            workspace_root: workspace_root.unwrap_or_else(|| tracker.path.display().to_string()),
            model: tracker
                .state
                .model
                .clone()
                .or_else(|| otel.and_then(|(_, session)| session.model.clone())),
            effort: tracker
                .state
                .effort
                .clone()
                .or_else(|| otel.and_then(|(_, session)| session.effort.clone())),
            context_left_percent: tracker.state.context_left_percent,
            status,
            tmux_running,
            attached_clients: tmux.map(|info| info.attached_clients).unwrap_or(0),
            last_activity_unix,
            last_event_name: choose_latest_event_name(
                tracker
                    .state
                    .last_activity_unix
                    .or(Some(tracker.modified_unix)),
                tracker.state.last_event_name.as_deref(),
                otel.and_then(|(_, session)| session.last_activity_unix),
                otel.and_then(|(_, session)| session.last_event_name.as_deref()),
            ),
            otel_events: otel
                .map(|(_, session)| session.otel_events)
                .unwrap_or_default(),
            api_requests: otel
                .map(|(_, session)| session.api_requests)
                .unwrap_or_default(),
            live_tool_calls: otel
                .map(|(_, session)| session.tool_calls)
                .unwrap_or_default(),
            lifetime_tool_calls: tracker.state.tool_calls,
            live_tool_errors: otel
                .map(|(_, session)| session.tool_errors)
                .unwrap_or_default(),
            lifetime_tool_errors: tracker.state.tool_errors,
            sse_events: otel
                .map(|(_, session)| session.sse_events)
                .unwrap_or_default(),
            live_otel,
        });
        seen_keys.insert(key);
        if let Some(session_id) = row_session_id {
            seen_keys.insert(session_id);
        }
    }

    for record in synthetic_records {
        let tmux = tmux_sessions.get(&record.session_name);
        let tmux_running = tmux.is_some();
        let otel = latest_workspace_otel_session(otel_state, &record.workspace_root);
        let row_session_id = otel.map(|(session_id, _)| session_id.clone());
        rows.push(CodexTopRow {
            key: format!("managed:{}", record.session_name),
            session_id: row_session_id.clone(),
            managed_session_name: Some(record.session_name.clone()),
            workspace_root: record.workspace_root.clone(),
            model: otel.and_then(|(_, session)| session.model.clone()),
            effort: otel.and_then(|(_, session)| session.effort.clone()),
            context_left_percent: None,
            status: session_status_label(tmux_running, true, tmux_available),
            tmux_running,
            attached_clients: tmux.map(|info| info.attached_clients).unwrap_or(0),
            last_activity_unix: select_latest_activity(
                tmux.and_then(|info| info.activity_unix)
                    .or(Some(record.created_at_unix)),
                otel.and_then(|(_, session)| session.last_activity_unix),
            ),
            last_event_name: choose_latest_event_name(
                tmux.and_then(|info| info.activity_unix)
                    .or(Some(record.created_at_unix)),
                Some(&format!("launcher:{}", record.launcher)),
                otel.and_then(|(_, session)| session.last_activity_unix),
                otel.and_then(|(_, session)| session.last_event_name.as_deref()),
            ),
            otel_events: otel
                .map(|(_, session)| session.otel_events)
                .unwrap_or_default(),
            api_requests: otel
                .map(|(_, session)| session.api_requests)
                .unwrap_or_default(),
            live_tool_calls: otel
                .map(|(_, session)| session.tool_calls)
                .unwrap_or_default(),
            lifetime_tool_calls: 0,
            live_tool_errors: otel
                .map(|(_, session)| session.tool_errors)
                .unwrap_or_default(),
            lifetime_tool_errors: 0,
            sse_events: otel
                .map(|(_, session)| session.sse_events)
                .unwrap_or_default(),
            live_otel: otel
                .and_then(|(_, session)| session.last_activity_unix)
                .is_some_and(|last| current_unix_seconds().saturating_sub(last) <= 5),
        });
        if let Some(session_id) = row_session_id {
            seen_keys.insert(session_id);
        }
    }

    for (session_id, otel) in &otel_state.sessions {
        if seen_keys.contains(session_id) {
            continue;
        }
        if !workspace_visible(
            show_all,
            current_workspace_root,
            otel.workspace_root.as_deref(),
        ) {
            continue;
        }
        rows.push(CodexTopRow {
            key: session_id.clone(),
            session_id: Some(session_id.clone()),
            managed_session_name: None,
            workspace_root: otel
                .workspace_root
                .clone()
                .unwrap_or_else(|| "<unknown workspace>".to_string()),
            model: otel.model.clone(),
            effort: otel.effort.clone(),
            context_left_percent: None,
            status: top_row_status(false, false, tmux_available, true, otel.last_activity_unix)
                .to_string(),
            tmux_running: false,
            attached_clients: 0,
            last_activity_unix: otel.last_activity_unix,
            last_event_name: otel.last_event_name.clone(),
            otel_events: otel.otel_events,
            api_requests: otel.api_requests,
            live_tool_calls: otel.tool_calls,
            lifetime_tool_calls: 0,
            live_tool_errors: otel.tool_errors,
            lifetime_tool_errors: 0,
            sse_events: otel.sse_events,
            live_otel: true,
        });
    }

    if !show_history {
        rows.retain(row_is_active_now);
    }

    rows.sort_by(|a, b| {
        top_status_rank(&a.status)
            .cmp(&top_status_rank(&b.status))
            .then_with(|| b.last_activity_unix.cmp(&a.last_activity_unix))
            .then_with(|| a.workspace_root.cmp(&b.workspace_root))
            .then_with(|| a.key.cmp(&b.key))
    });
    rows
}

fn latest_workspace_otel_session<'a>(
    otel_state: &'a OtelLiveState,
    workspace_root: &str,
) -> Option<(&'a String, &'a OtelSessionState)> {
    otel_state
        .sessions
        .iter()
        .filter(|(_, session)| session.workspace_root.as_deref() == Some(workspace_root))
        .max_by(|(left_id, left), (right_id, right)| {
            left.last_activity_unix
                .cmp(&right.last_activity_unix)
                .then_with(|| left.otel_events.cmp(&right.otel_events))
                .then_with(|| left_id.cmp(right_id))
        })
}

fn pick_preferred_row_otel<'a>(
    tracker_otel: Option<(&'a String, &'a OtelSessionState)>,
    workspace_otel: Option<(&'a String, &'a OtelSessionState)>,
) -> Option<(&'a String, &'a OtelSessionState)> {
    match (tracker_otel, workspace_otel) {
        (Some((_, tracker)), Some((workspace_id, workspace)))
            if workspace.last_activity_unix >= tracker.last_activity_unix =>
        {
            Some((workspace_id, workspace))
        }
        (Some((tracker_id, tracker)), Some(_)) => Some((tracker_id, tracker)),
        (None, Some((workspace_id, workspace))) => Some((workspace_id, workspace)),
        (Some((tracker_id, tracker)), None) => Some((tracker_id, tracker)),
        (None, None) => None,
    }
}

fn preferred_stream_row<'a>(
    rows: &'a [CodexTopRow],
    current_session_id: &str,
    workspace_root: &str,
    otel_state: &OtelLiveState,
) -> Option<&'a CodexTopRow> {
    if otel_state.sessions.contains_key(current_session_id) {
        return None;
    }

    rows.iter()
        .filter(|row| row.workspace_root == workspace_root)
        .filter(|row| row.session_id.as_deref() != Some(current_session_id))
        .filter(|row| row.session_id.is_some())
        .filter(|row| row.otel_events > 0 || row.live_otel)
        .max_by(|left, right| {
            left.live_otel
                .cmp(&right.live_otel)
                .then_with(|| left.last_activity_unix.cmp(&right.last_activity_unix))
                .then_with(|| left.otel_events.cmp(&right.otel_events))
                .then_with(|| left.key.cmp(&right.key))
        })
}

fn best_tracker_match_for_record(
    record: &SessionRecord,
    trackers: &[&SessionFileTracker],
    assigned_tracker_keys: &BTreeSet<String>,
) -> Option<String> {
    let candidates = trackers
        .iter()
        .filter(|tracker| {
            tracker.state.workspace_root.as_deref() == Some(record.workspace_root.as_str())
        })
        .filter(|tracker| !assigned_tracker_keys.contains(&tracker.key()))
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return None;
    }

    let created_at_unix = record.created_at_unix;

    if let Some((tracker_key, _, _)) = candidates
        .iter()
        .filter_map(|tracker| {
            let tracker_key = tracker.key();
            let reference_unix = tracker_match_reference_unix(tracker);
            let delta = reference_unix.checked_sub(created_at_unix)?;
            (delta <= MANAGED_TRACKER_MATCH_WINDOW_SECS).then_some((
                tracker_key,
                delta,
                reference_unix,
            ))
        })
        .min_by(|a, b| {
            a.1.cmp(&b.1)
                .then_with(|| b.2.cmp(&a.2))
                .then_with(|| a.0.cmp(&b.0))
        })
    {
        return Some(tracker_key);
    }

    if let Some((tracker_key, _, _)) = candidates
        .iter()
        .filter_map(|tracker| {
            let tracker_key = tracker.key();
            let reference_unix = tracker_match_reference_unix(tracker);
            let diff = reference_unix.abs_diff(created_at_unix);
            (diff <= MANAGED_TRACKER_MATCH_WINDOW_SECS).then_some((
                tracker_key,
                diff,
                reference_unix,
            ))
        })
        .min_by(|a, b| {
            a.1.cmp(&b.1)
                .then_with(|| b.2.cmp(&a.2))
                .then_with(|| a.0.cmp(&b.0))
        })
    {
        return Some(tracker_key);
    }

    candidates
        .iter()
        .map(|tracker| (tracker.key(), tracker_match_reference_unix(tracker)))
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
        .map(|(tracker_key, _)| tracker_key)
}

fn tracker_match_reference_unix(tracker: &SessionFileTracker) -> u64 {
    tracker
        .state
        .started_unix
        .unwrap_or_default()
        .max(tracker.state.last_activity_unix.unwrap_or_default())
        .max(tracker.modified_unix)
}

fn workspace_visible(
    show_all: bool,
    current_workspace_root: &str,
    workspace_root: Option<&str>,
) -> bool {
    if show_all {
        return true;
    }
    workspace_root == Some(current_workspace_root)
}

fn top_row_status(
    tmux_running: bool,
    managed: bool,
    tmux_available: bool,
    live_otel: bool,
    last_activity_unix: Option<u64>,
) -> &'static str {
    if tmux_running {
        return "running";
    }
    if live_otel {
        return "live";
    }
    if managed && !tmux_available {
        return "unavailable";
    }
    if managed {
        return "stale";
    }
    let elapsed = last_activity_unix.map(|unix| current_unix_seconds().saturating_sub(unix));
    if elapsed.is_some_and(|elapsed| elapsed <= 60) {
        "idle"
    } else {
        "ended"
    }
}

fn top_status_rank(status: &str) -> usize {
    match status {
        "running" => 0,
        "live" => 1,
        "idle" => 2,
        "stale" => 3,
        "unavailable" => 4,
        _ => 5,
    }
}

fn row_is_active_now(row: &CodexTopRow) -> bool {
    row.tmux_running || row.live_otel
}

fn select_latest_activity(file_activity: Option<u64>, otel_activity: Option<u64>) -> Option<u64> {
    match (file_activity, otel_activity) {
        (Some(file), Some(otel)) => Some(file.max(otel)),
        (Some(file), None) => Some(file),
        (None, Some(otel)) => Some(otel),
        (None, None) => None,
    }
}

fn choose_latest_event_name(
    file_activity: Option<u64>,
    file_name: Option<&str>,
    otel_activity: Option<u64>,
    otel_name: Option<&str>,
) -> Option<String> {
    match (file_activity, file_name, otel_activity, otel_name) {
        (_, _, Some(otel_activity), Some(otel_name))
            if otel_activity >= file_activity.unwrap_or_default() =>
        {
            Some(otel_name.to_string())
        }
        (_, Some(file_name), _, _) => Some(file_name.to_string()),
        (_, _, _, Some(otel_name)) => Some(otel_name.to_string()),
        _ => None,
    }
}

fn draw_top_tui(
    frame: &mut ratatui::Frame<'_>,
    app: &mut CodexTopApp,
    listener: &TopListenerHandle,
) {
    if matches!(app.view, TopView::Stream(_)) {
        draw_stream_tui(frame, app, listener);
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(8),
            Constraint::Length(8),
        ])
        .split(frame.area());

    let live_rows = app
        .rows
        .iter()
        .filter(|row| matches!(row.status.as_str(), "running" | "live"))
        .count();
    let overview = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                "za codex top",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::raw(format!(
                "scope={}  history={}  visible={}  live={}  total-otel={}",
                if app.show_all { "all" } else { "workspace" },
                if app.show_history { "on" } else { "off" },
                app.rows.len(),
                live_rows,
                app.otel_state.total_events
            )),
        ]),
        Line::from(Span::raw(format!("listener={}", listener.endpoint))),
        Line::from(Span::raw(format!(
            "last-otel={}  current-workspace={}",
            activity_age_label(app.otel_state.last_event_unix),
            truncate_end(
                &app.current_workspace_root,
                usize::from(chunks[0].width.saturating_sub(4)).max(1)
            ),
        ))),
    ])
    .block(Block::default().borders(Borders::ALL).title("Overview"));
    frame.render_widget(overview, chunks[0]);

    let sessions_block = Block::default().borders(Borders::ALL).title("Sessions");
    let inner = sessions_block.inner(chunks[1]);
    frame.render_widget(sessions_block, chunks[1]);

    let session_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);
    let header = Paragraph::new(top_rows_header_line()).style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_widget(header, session_chunks[0]);

    let items = if app.rows.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            if app.show_history {
                "No Codex sessions matched the current scope."
            } else {
                "No active Codex sessions matched the current scope. Press `h` to include history."
            },
            Style::default().fg(Color::DarkGray),
        )))]
    } else {
        app.rows.iter().map(top_row_item).collect::<Vec<_>>()
    };
    let mut list_state = ListState::default()
        .with_offset(app.scroll_offset)
        .with_selected((!app.rows.is_empty()).then_some(app.selected));
    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, session_chunks[1], &mut list_state);
    app.scroll_offset = list_state.offset();
    app.viewport_rows = usize::from(session_chunks[1].height.max(1));

    let detail = match app.rows.get(app.selected) {
        Some(row) => top_detail_lines(row, &app.status_message),
        None => vec![
            Line::from("j/k move  PgUp/PgDn page  Enter stream  a scope  h history  q quit"),
            Line::from(app.status_message.clone().unwrap_or_else(|| {
                "Launching `za codex` while this screen is open will auto-enable live OTLP streaming.".to_string()
            })),
        ],
    };
    let detail =
        Paragraph::new(detail).block(Block::default().borders(Borders::ALL).title("Detail"));
    frame.render_widget(detail, chunks[2]);
}

fn top_rows_header_line() -> Line<'static> {
    Line::from(format!(
        "{:<4} {:<4} {:<6} {:<5} {:<18} {:>3} {:>5} {:>5} {:>7} {:<12} {}",
        "TMUX",
        "LIVE",
        "ACTIVE",
        "LEFT",
        "MODEL/EFFORT",
        "API",
        "TLIVE",
        "TLIFE",
        "ERR L/A",
        "SESSION",
        "WORKSPACE"
    ))
}

fn top_row_item(row: &CodexTopRow) -> ListItem<'static> {
    let model = match (&row.model, &row.effort) {
        (Some(model), Some(effort)) => format!("{model}/{effort}"),
        (Some(model), None) => model.clone(),
        (None, Some(effort)) => format!("-/{effort}"),
        (None, None) => "-".to_string(),
    };
    let tmux_label = if row.tmux_running { "yes" } else { "-" };
    let live_label = if row.live_otel { "yes" } else { "-" };
    let err_label = format!("{}/{}", row.live_tool_errors, row.lifetime_tool_errors);
    let line = format!(
        "{:<4} {:<4} {:<6} {:<5} {:<18} {:>3} {:>5} {:>5} {:>7} {:<12} {}",
        tmux_label,
        live_label,
        activity_age_label(row.last_activity_unix),
        format_left_percent(row.context_left_percent),
        truncate_end(&model, 18),
        row.api_requests,
        row.live_tool_calls,
        row.lifetime_tool_calls,
        err_label,
        truncate_end(row.session_id.as_deref().unwrap_or("-"), 12),
        truncate_end(&row.workspace_root, 80),
    );
    ListItem::new(Line::from(vec![
        Span::styled(
            format!("{:<4}", tmux_label),
            top_status_style(&row.status, row.live_otel),
        ),
        Span::raw(line[4..].to_string()),
    ]))
}

fn top_status_style(status: &str, live_otel: bool) -> Style {
    let base = match status {
        "running" => Style::default().fg(Color::Green),
        "live" => Style::default().fg(Color::Cyan),
        "idle" => Style::default().fg(Color::Yellow),
        "stale" | "unavailable" => Style::default().fg(Color::Red),
        _ => Style::default().fg(Color::DarkGray),
    };
    if live_otel {
        base.add_modifier(Modifier::BOLD)
    } else {
        base
    }
}

fn top_detail_lines(row: &CodexTopRow, status_message: &Option<String>) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(Line::from(format!(
        "status={}  tmux={}  live={}  session={}  managed={}",
        row.status,
        if row.tmux_running { "yes" } else { "no" },
        if row.live_otel { "yes" } else { "no" },
        row.session_id.as_deref().unwrap_or("-"),
        row.managed_session_name.as_deref().unwrap_or("-"),
    )));
    lines.push(Line::from(format!(
        "workspace={}  clients={}  last={}",
        row.workspace_root,
        row.attached_clients,
        row.last_event_name.as_deref().unwrap_or("-"),
    )));
    lines.push(Line::from(format!(
        "model={}  left={}  api={}  otel={}  sse={}",
        row.model.as_deref().unwrap_or("-"),
        format_left_percent(row.context_left_percent),
        row.api_requests,
        row.otel_events,
        row.sse_events,
    )));
    lines.push(Line::from(format!(
        "tool_live={}  tool_life={}  err_live={}  err_life={}  Enter stream  a scope  h history  q quit",
        row.live_tool_calls,
        row.lifetime_tool_calls,
        row.live_tool_errors,
        row.lifetime_tool_errors,
    )));
    lines.push(Line::from(format!(
        "effort={}",
        row.effort.as_deref().unwrap_or("-"),
    )));
    if let Some(message) = status_message {
        lines.push(Line::from(message.clone()));
    }
    lines
}

fn draw_stream_tui(
    frame: &mut ratatui::Frame<'_>,
    app: &mut CodexTopApp,
    listener: &TopListenerHandle,
) {
    let (
        session_id,
        workspace_root,
        model,
        effort,
        tmux_running,
        live_otel,
        follow,
        scroll_offset,
        selected,
    ) = match &app.view {
        TopView::Stream(stream) => (
            stream.session_id.clone(),
            stream.workspace_root.clone(),
            stream.model.clone(),
            stream.effort.clone(),
            stream.tmux_running,
            stream.live_otel,
            stream.follow,
            stream.scroll_offset,
            stream.selected,
        ),
        TopView::Summary => return,
    };

    let events = app.stream_event_vec(&session_id);
    let resolved_selected = if follow {
        events.len().saturating_sub(1)
    } else {
        selected.min(events.len().saturating_sub(1))
    };
    app.update_stream_state(|stream| {
        stream.selected = resolved_selected;
    });

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(8),
            Constraint::Length(8),
        ])
        .split(frame.area());

    let summary_row = app
        .rows
        .iter()
        .find(|row| row.session_id.as_deref() == Some(session_id.as_str()));
    let overview = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                "za codex top / stream",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::raw(format!(
                "events={}  follow={}  listener={}",
                events.len(),
                if follow { "on" } else { "off" },
                truncate_end(&listener.endpoint, 24)
            )),
        ]),
        Line::from(Span::raw(format!(
            "session={}  workspace={}",
            session_id,
            summary_row
                .map(|row| row.workspace_root.as_str())
                .unwrap_or(workspace_root.as_str())
        ))),
        Line::from(Span::raw(format!(
            "model={}  effort={}  tmux={}  live={}",
            summary_row
                .and_then(|row| row.model.as_deref())
                .unwrap_or(model.as_deref().unwrap_or("-")),
            summary_row
                .and_then(|row| row.effort.as_deref())
                .unwrap_or(effort.as_deref().unwrap_or("-")),
            if summary_row.is_some_and(|row| row.tmux_running)
                || summary_row.is_none() && tmux_running
            {
                "yes"
            } else {
                "no"
            },
            if summary_row.is_some_and(|row| row.live_otel) || summary_row.is_none() && live_otel {
                "yes"
            } else {
                "no"
            }
        ))),
    ])
    .block(Block::default().borders(Borders::ALL).title("Event Stream"));
    frame.render_widget(overview, chunks[0]);

    let stream_block = Block::default().borders(Borders::ALL).title("Events");
    let inner = stream_block.inner(chunks[1]);
    frame.render_widget(stream_block, chunks[1]);
    let stream_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);
    let header = Paragraph::new(stream_rows_header_line()).style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_widget(header, stream_chunks[0]);

    let items = if events.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "No live OTel events captured for this session yet.",
            Style::default().fg(Color::DarkGray),
        )))]
    } else {
        events
            .iter()
            .map(|event| stream_row_item(event, stream_chunks[1].width))
            .collect::<Vec<_>>()
    };
    let mut list_state = ListState::default()
        .with_offset(scroll_offset)
        .with_selected((!events.is_empty()).then_some(resolved_selected));
    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, stream_chunks[1], &mut list_state);
    app.update_stream_state(|stream| {
        stream.scroll_offset = list_state.offset();
        stream.viewport_rows = usize::from(stream_chunks[1].height.max(1));
    });

    let detail = events
        .get(resolved_selected)
        .map(|event| stream_detail_lines(event, &app.status_message))
        .unwrap_or_else(|| {
            vec![
                Line::from("Esc back  f follow  j/k move  PgUp/PgDn page  q quit"),
                Line::from(app.status_message.clone().unwrap_or_else(|| {
                    "Waiting for the selected session to emit new OTel events.".to_string()
                })),
            ]
        });
    let detail =
        Paragraph::new(detail).block(Block::default().borders(Borders::ALL).title("Event Detail"));
    frame.render_widget(detail, chunks[2]);
}

fn stream_rows_header_line() -> Line<'static> {
    Line::from(format!(
        "{:<6} {:<5} {:<28} {}",
        "ACTIVE", "ERR", "EVENT", "ATTRS"
    ))
}

fn stream_row_item(event: &OtelEventRecord, width: u16) -> ListItem<'static> {
    let snippet_width = usize::from(width.saturating_sub(2)).saturating_sub(44);
    let snippet = truncate_end(&stream_event_snippet(event), snippet_width.max(12));
    let line = format!(
        "{:<6} {:<5} {:<28} {}",
        activity_age_label(Some(event.observed_unix)),
        if event.tool_error { "yes" } else { "-" },
        truncate_end(&event.event_name, 28),
        snippet,
    );
    ListItem::new(Line::from(vec![
        Span::styled(
            format!("{:<6}", activity_age_label(Some(event.observed_unix))),
            if event.tool_error {
                Style::default().fg(Color::Red)
            } else {
                Style::default()
            },
        ),
        Span::raw(line[6..].to_string()),
    ]))
}

fn stream_event_snippet(event: &OtelEventRecord) -> String {
    let mut fields = Vec::new();
    for (key, value) in &event.attributes {
        if matches!(
            key.as_str(),
            "conversation.id"
                | "event.name"
                | "event.timestamp"
                | "model"
                | "slug"
                | "reasoning_effort"
                | "cwd"
                | "workspace"
                | "workspace_root"
        ) {
            continue;
        }
        fields.push(format!("{key}={value}"));
        if fields.len() >= 3 {
            break;
        }
    }
    if fields.is_empty() {
        event.body.clone().unwrap_or_else(|| "-".to_string())
    } else {
        fields.join("  ")
    }
}

fn stream_detail_lines(
    event: &OtelEventRecord,
    status_message: &Option<String>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(Line::from(format!(
        "event={}  active={}  error={}  attrs={}  Esc back  f follow  q quit",
        event.event_name,
        activity_age_label(Some(event.observed_unix)),
        if event.tool_error { "yes" } else { "no" },
        event.attributes.len(),
    )));
    if let Some(body) = &event.body {
        lines.push(Line::from(format!("body={}", truncate_end(body, 120))));
    }
    let mut attr_lines = Vec::new();
    for (key, value) in &event.attributes {
        attr_lines.push(format!("{key}={value}"));
    }
    if attr_lines.is_empty() {
        lines.push(Line::from("attributes: -"));
    } else {
        for chunk in attr_lines.chunks(2).take(4) {
            lines.push(Line::from(chunk.join("    ")));
        }
    }
    if let Some(message) = status_message {
        lines.push(Line::from(message.clone()));
    }
    lines
}

fn parse_codex_session_event(line: &str) -> Option<ParsedCodexSessionEvent> {
    let event = serde_json::from_str::<CodexLogEventEnvelope>(line).ok()?;
    match event.kind.as_str() {
        "session_meta" => serde_json::from_value::<SessionMetaPayload>(event.payload)
            .ok()
            .map(ParsedCodexSessionEvent::SessionMeta),
        "turn_context" => serde_json::from_value::<TurnContextPayload>(event.payload)
            .ok()
            .map(ParsedCodexSessionEvent::TurnContext),
        "token_count" => serde_json::from_value::<TokenCountPayload>(event.payload)
            .ok()
            .map(|payload| ParsedCodexSessionEvent::TokenCount(payload.info)),
        "event_msg" => {
            let payload = serde_json::from_value::<CodexEventMessagePayload>(event.payload).ok()?;
            if payload.kind.as_deref() != Some("token_count") {
                return None;
            }
            payload.info.map(ParsedCodexSessionEvent::TokenCount)
        }
        _ => None,
    }
}

fn summarize_codex_session_file(
    path: &Path,
    workspace_starts: &BTreeMap<String, u64>,
) -> Result<Option<CodexSessionSummary>> {
    let modified_unix = fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or_default();

    let file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    summarize_codex_session_lines(BufReader::new(file), modified_unix, workspace_starts)
}

fn summarize_codex_session_lines<R: BufRead>(
    reader: R,
    modified_unix: u64,
    workspace_starts: &BTreeMap<String, u64>,
) -> Result<Option<CodexSessionSummary>> {
    let mut session_id = None;
    let mut workspace_root = None;
    let mut model = None;
    let mut effort = None;
    let mut context_left_percent = None;

    for line in reader.lines() {
        let line = line.context("read codex session log line")?;
        let Some(event) = parse_codex_session_event(&line) else {
            continue;
        };
        match event {
            ParsedCodexSessionEvent::SessionMeta(payload) => {
                let cwd = payload.cwd.trim();
                let Some(started_unix) = workspace_starts.get(cwd) else {
                    return Ok(None);
                };
                if modified_unix + 300 < *started_unix {
                    return Ok(None);
                }
                session_id = Some(payload.id);
                workspace_root = Some(cwd.to_string());
            }
            ParsedCodexSessionEvent::TurnContext(payload) => {
                if let Some(value) = payload
                    .model
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
                {
                    model = Some(value);
                }
                if let Some(value) = payload
                    .effort
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
                {
                    effort = Some(value);
                }
            }
            ParsedCodexSessionEvent::TokenCount(info) => {
                context_left_percent = calculate_context_left_percent(
                    info.last_token_usage.total_tokens,
                    info.model_context_window,
                );
            }
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

fn calculate_context_left_percent(used_tokens: u64, context_window: u64) -> Option<f64> {
    if context_window == 0 {
        return None;
    }
    let used_percent = (used_tokens as f64 / context_window as f64) * 100.0;
    Some((100.0 - used_percent).clamp(0.0, 100.0))
}

fn parse_legacy_codex_context_left_percent_lines<R: BufRead>(
    reader: R,
    session_ids: &BTreeSet<String>,
) -> Result<BTreeMap<String, f64>> {
    let thread_re = Regex::new(r"thread_id=([0-9a-f-]{36})").expect("valid thread regex");
    let usage_re = Regex::new(r"estimated_token_count=Some\((\d+)\).*auto_compact_limit=(\d+)")
        .expect("valid usage regex");

    let mut usage = BTreeMap::new();
    for line in reader.lines() {
        let line = line.context("read codex tui log line")?;
        if !line.contains("estimated_token_count=Some(") || !line.contains("auto_compact_limit=") {
            continue;
        }
        let Some(thread) = thread_re
            .captures(&line)
            .and_then(|caps| caps.get(1))
            .map(|m| m.as_str().to_string())
        else {
            continue;
        };
        if !session_ids.contains(&thread) {
            continue;
        }
        let Some((estimated, limit)) = usage_re.captures(&line).and_then(|caps| {
            let estimated = caps.get(1)?.as_str().parse::<f64>().ok()?;
            let limit = caps.get(2)?.as_str().parse::<f64>().ok()?;
            Some((estimated, limit))
        }) else {
            continue;
        };
        if let Some(left_percent) = calculate_context_left_percent(estimated as u64, limit as u64) {
            usage.insert(thread, left_percent);
        }
    }
    Ok(usage)
}

fn list_tmux_sessions() -> Result<BTreeMap<String, TmuxSessionInfo>> {
    let output = Command::new("tmux")
        .args([
            "list-sessions",
            "-F",
            "#{session_name}\t#{session_created}\t#{session_activity}\t#{session_attached}",
        ])
        .output()
        .context("list tmux sessions")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if is_tmux_no_server(&stderr) {
            return Ok(BTreeMap::new());
        }
        bail!("`tmux list-sessions` failed: {}", stderr.trim());
    }
    parse_tmux_sessions(&String::from_utf8_lossy(&output.stdout))
}

fn parse_tmux_sessions(raw: &str) -> Result<BTreeMap<String, TmuxSessionInfo>> {
    let mut sessions = BTreeMap::new();
    for line in raw.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let mut fields = line.split('\t');
        let name = fields
            .next()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("invalid tmux session line: missing name"))?
            .to_string();
        let created_unix = fields.next().and_then(parse_u64_field);
        let activity_unix = fields.next().and_then(parse_u64_field);
        let attached_clients = fields
            .next()
            .and_then(parse_usize_field)
            .unwrap_or_default();

        sessions.insert(
            name.clone(),
            TmuxSessionInfo {
                created_unix,
                activity_unix,
                attached_clients,
            },
        );
    }
    Ok(sessions)
}

fn parse_u64_field(value: &str) -> Option<u64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse().ok()
}

fn parse_usize_field(value: &str) -> Option<usize> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse().ok()
}

fn session_status_label(running: bool, metadata_present: bool, tmux_available: bool) -> String {
    if running {
        "running".to_string()
    } else if metadata_present && !tmux_available {
        "unavailable".to_string()
    } else if metadata_present {
        "stale".to_string()
    } else {
        "unknown".to_string()
    }
}

fn no_managed_sessions_message(tmux_available: bool) -> &'static str {
    if tmux_available {
        "No managed Codex sessions found."
    } else {
        "No managed Codex sessions found. (`tmux` unavailable.)"
    }
}

fn render_stop_message(output: &CodexStopOutput) -> String {
    if output.stopped {
        return format!("Stopped Codex session `{}`.", output.session_name);
    }
    if !output.tmux_available {
        if output.metadata_removed {
            return format!(
                "Removed local Codex session metadata for `{}`; `tmux` is unavailable, so the underlying session was not stopped.",
                output.session_name
            );
        }
        return format!(
            "No local Codex session metadata found for `{}`; `tmux` is unavailable, so no session could be checked.",
            output.session_name
        );
    }
    if output.metadata_removed {
        return format!(
            "Removed stale Codex session metadata for `{}`; no running tmux session was found.",
            output.session_name
        );
    }
    format!(
        "No managed Codex session found for `{}`.",
        output.workspace_root
    )
}

fn sanitize_session_label(raw: &str) -> String {
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

fn next_exec_window_name() -> String {
    format!("exec-{}", current_unix_seconds())
}

fn current_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn activity_age_label(activity_unix: Option<u64>) -> String {
    let Some(activity_unix) = activity_unix else {
        return "-".to_string();
    };
    let now = current_unix_seconds();
    let elapsed = now.saturating_sub(activity_unix);
    if elapsed < 60 {
        format!("{elapsed}s")
    } else if elapsed < 3_600 {
        format!("{}m", elapsed / 60)
    } else if elapsed < 86_400 {
        format!("{}h", elapsed / 3_600)
    } else {
        format!("{}d", elapsed / 86_400)
    }
}

fn format_left_percent(value: Option<f64>) -> String {
    match value {
        Some(value) => format!("{value:.0}%"),
        None => "-".to_string(),
    }
}

fn truncate_end(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let mut out = String::new();
    for c in value.chars().take(max.saturating_sub(1)) {
        out.push(c);
    }
    out.push('…');
    out
}

fn is_tmux_no_server(stderr: &str) -> bool {
    let lower = stderr.trim().to_ascii_lowercase();
    lower.contains("failed to connect to server")
        || lower.contains("no server running")
        || (lower.contains("error connecting to") && lower.contains("no such file or directory"))
}

fn is_tmux_missing_session(stderr: &str) -> bool {
    stderr
        .trim()
        .to_ascii_lowercase()
        .contains("can't find session")
}

fn is_tmux_session_absent(stderr: &str) -> bool {
    is_tmux_missing_session(stderr) || is_tmux_no_server(stderr)
}

fn git_capture(path: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .with_context(|| format!("run `git {}`", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("`git {}` failed: {}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        CodexStopOutput, CodexTopApp, CodexTopRow, FileSessionState, OtelEventRecord,
        OtelLiveState, OtelSessionState, SESSION_HASH_LEN, SessionFileTracker, SessionRecord,
        TmuxSessionInfo, TopRowsInput, TopStreamState, TopView, activity_age_label,
        apply_session_log_line, best_tracker_match_for_record, build_shell_exec_command,
        build_top_rows, calculate_context_left_percent, config_overrides_otel,
        ensure_local_listener_no_proxy, is_tmux_no_server, is_tmux_session_absent,
        parse_legacy_codex_context_left_percent_lines, parse_otlp_session_events,
        parse_tmux_codex_window_ids, parse_tmux_sessions, render_stop_message, resolve_state_home,
        sanitize_session_label, session_status_label, shell_escape, summarize_codex_session_lines,
        tmux_panes_include_listener_endpoint, tmux_terminal_overrides_disable_alt_screen,
        workspace_hash,
    };
    use std::{
        collections::{BTreeMap, BTreeSet, VecDeque},
        io::Cursor,
        path::{Path, PathBuf},
    };

    #[test]
    fn sanitize_session_label_normalizes_and_truncates() {
        assert_eq!(sanitize_session_label("Reqx RS"), "reqx-rs");
        assert_eq!(
            sanitize_session_label("This.Is A Very Long Workspace Name"),
            "this-is-a-very-long-work"
        );
        assert_eq!(sanitize_session_label("___"), "workspace");
    }

    #[test]
    fn workspace_hash_is_stable_hex() {
        let hash = workspace_hash(Path::new("/opt/app/za"));
        assert_eq!(hash.len(), 64);
        assert_eq!(
            &hash[..SESSION_HASH_LEN],
            &workspace_hash(Path::new("/opt/app/za"))[..SESSION_HASH_LEN]
        );
    }

    #[test]
    fn shell_escape_handles_quotes_and_empty_strings() {
        assert_eq!(shell_escape(""), "''");
        assert_eq!(shell_escape("plain"), "'plain'");
        assert_eq!(shell_escape("it's"), "'it'\"'\"'s'");
    }

    #[test]
    fn build_shell_exec_command_prefixes_env_and_exec() {
        let env_vars = vec![("HTTPS_PROXY".to_string(), "http://proxy:7890".to_string())];
        let argv = vec![
            "/usr/local/bin/codex".to_string(),
            "--no-alt-screen".to_string(),
        ];
        let command = build_shell_exec_command(&env_vars, &argv).expect("must build command");
        assert_eq!(
            command,
            "exec env HTTPS_PROXY='http://proxy:7890' '/usr/local/bin/codex' '--no-alt-screen'"
        );
    }

    #[test]
    fn config_overrides_otel_detects_otel_keys() {
        assert!(config_overrides_otel(
            "otel.exporter={otlp-http={endpoint=\"http://127.0.0.1:4318/v1/logs\"}}"
        ));
        assert!(config_overrides_otel("otel.log_user_prompt=false"));
        assert!(!config_overrides_otel("model=\"gpt-5.4\""));
    }

    #[test]
    fn ensure_local_listener_no_proxy_adds_loopback_rules() {
        let mut env_vars = vec![(
            "HTTPS_PROXY".to_string(),
            "http://proxy.internal:7890".to_string(),
        )];
        ensure_local_listener_no_proxy(&mut env_vars);
        assert!(
            env_vars
                .iter()
                .any(|(key, value)| { key == "NO_PROXY" && value == "127.0.0.1,localhost,::1" })
        );
        assert!(
            env_vars
                .iter()
                .any(|(key, value)| { key == "no_proxy" && value == "127.0.0.1,localhost,::1" })
        );
    }

    #[test]
    fn ensure_local_listener_no_proxy_preserves_existing_rules() {
        let mut env_vars = vec![(
            "NO_PROXY".to_string(),
            "corp.internal,localhost".to_string(),
        )];
        ensure_local_listener_no_proxy(&mut env_vars);
        assert!(env_vars.iter().any(|(key, value)| {
            key == "NO_PROXY" && value == "corp.internal,localhost,127.0.0.1,::1"
        }));
        assert!(env_vars.iter().any(|(key, value)| {
            key == "no_proxy" && value == "corp.internal,localhost,127.0.0.1,::1"
        }));
    }

    #[test]
    fn parse_tmux_sessions_reads_expected_fields() {
        let sessions = parse_tmux_sessions(
            "za-codex-za-123\t1700000000\t1700000300\t1\nza-codex-api-456\t1700000100\t1700000200\t0\n",
        )
        .expect("must parse");
        let first = sessions.get("za-codex-za-123").expect("first session");
        assert_eq!(first.created_unix, Some(1_700_000_000));
        assert_eq!(first.activity_unix, Some(1_700_000_300));
        assert_eq!(first.attached_clients, 1);
    }

    #[test]
    fn tmux_panes_include_listener_endpoint_matches_codex_start_command() {
        let output = concat!(
            "bash\tbash\n",
            "codex\t\"exec env NO_PROXY='127.0.0.1,localhost,::1' '/usr/local/bin/codex' '--no-alt-screen' '-c' 'otel.exporter={otlp-http={endpoint=\\\"http://127.0.0.1:45553/v1/logs\\\",protocol=\\\"json\\\"}}'\"\n"
        );
        assert!(tmux_panes_include_listener_endpoint(
            output,
            "http://127.0.0.1:45553/v1/logs"
        ));
        assert!(!tmux_panes_include_listener_endpoint(
            output,
            "http://127.0.0.1:45554/v1/logs"
        ));
    }

    #[test]
    fn parse_tmux_codex_window_ids_extracts_unique_codex_windows() {
        let output = concat!("codex\t@1\n", "bash\t@2\n", "codex\t@1\n", "codex\t@3\n");

        let window_ids = parse_tmux_codex_window_ids(output);

        assert_eq!(
            window_ids,
            BTreeSet::from(["@1".to_string(), "@3".to_string()])
        );
    }

    #[test]
    fn tmux_terminal_overrides_disable_alt_screen_detects_override() {
        let output = "terminal-overrides[0] *:smcup@:rmcup@\n";
        assert!(tmux_terminal_overrides_disable_alt_screen(output));
    }

    #[test]
    fn tmux_terminal_overrides_disable_alt_screen_ignores_unrelated_override() {
        let output = "terminal-overrides[0] xterm*:focus:title\n";
        assert!(!tmux_terminal_overrides_disable_alt_screen(output));
    }

    #[test]
    fn activity_age_label_compacts_elapsed_time() {
        assert_eq!(activity_age_label(None), "-");
    }

    #[test]
    fn session_status_label_marks_metadata_as_unavailable_without_tmux() {
        assert_eq!(session_status_label(false, true, false), "unavailable");
        assert_eq!(session_status_label(false, true, true), "stale");
        assert_eq!(session_status_label(true, true, false), "running");
    }

    #[test]
    fn render_stop_message_explains_tmux_missing_cleanup() {
        let message = render_stop_message(&CodexStopOutput {
            session_name: "za-codex-za-123".to_string(),
            workspace_root: "/opt/app/za".to_string(),
            stopped: false,
            metadata_removed: true,
            tmux_available: false,
            note: Some("`tmux` is not installed; removed local session metadata only".to_string()),
        });
        assert!(message.contains("Removed local Codex session metadata"));
        assert!(message.contains("tmux` is unavailable"));
    }

    #[test]
    fn tmux_socket_missing_is_treated_as_no_server() {
        assert!(is_tmux_no_server(
            "error connecting to /tmp/tmux-0/default (No such file or directory)"
        ));
    }

    #[test]
    fn tmux_missing_session_or_server_is_treated_as_absent() {
        assert!(is_tmux_session_absent(
            "can't find session: za-codex-za-123"
        ));
        assert!(is_tmux_session_absent(
            "error connecting to /tmp/tmux-0/default (No such file or directory)"
        ));
    }

    #[test]
    fn resolve_state_home_prefers_xdg_without_home() {
        assert_eq!(
            resolve_state_home(Some(PathBuf::from("/tmp/state")), None).expect("must resolve"),
            PathBuf::from("/tmp/state")
        );
    }

    #[test]
    fn summarize_codex_session_lines_extracts_id_model_effort_and_context_left() {
        let workspaces = BTreeMap::from([("/opt/app/za".to_string(), 1_700_000_000)]);
        let raw = concat!(
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"019cc38e-4d75-7052-b96a-b3a1e36b1868\",\"cwd\":\"/opt/app/za\"}}\n",
            "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5.4\",\"effort\":\"xhigh\"}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"total_tokens\":181807},\"model_context_window\":258400}}}\n"
        );
        let summary = summarize_codex_session_lines(Cursor::new(raw), 1_700_000_100, &workspaces)
            .expect("must parse")
            .expect("must match workspace");
        assert_eq!(summary.session_id, "019cc38e-4d75-7052-b96a-b3a1e36b1868");
        assert_eq!(summary.workspace_root, "/opt/app/za");
        assert_eq!(summary.model.as_deref(), Some("gpt-5.4"));
        assert_eq!(summary.effort.as_deref(), Some("xhigh"));
        let pct = summary
            .context_left_percent
            .expect("left percent must exist");
        assert!(pct > 29.0 && pct < 30.0);
    }

    #[test]
    fn parse_otlp_session_events_extracts_core_attributes() {
        let raw = br#"{
          "resourceLogs":[
            {
              "scopeLogs":[
                {
                  "logRecords":[
                    {
                      "observedTimeUnixNano":"1773047144363148565",
                      "attributes":[
                        {"key":"event.name","value":{"stringValue":"codex.conversation_starts"}},
                        {"key":"conversation.id","value":{"stringValue":"019cd1d8-5fa8-7202-bcf6-42b2748dcb88"}},
                        {"key":"model","value":{"stringValue":"gpt-5.4"}},
                        {"key":"reasoning_effort","value":{"stringValue":"xhigh"}}
                      ]
                    },
                    {
                      "observedTimeUnixNano":"1773047145363148565",
                      "attributes":[
                        {"key":"event.name","value":{"stringValue":"codex.tool_result"}},
                        {"key":"conversation.id","value":{"stringValue":"019cd1d8-5fa8-7202-bcf6-42b2748dcb88"}},
                        {"key":"success","value":{"boolValue":false}}
                      ]
                    }
                  ]
                }
              ]
            }
          ]
        }"#;
        let events = parse_otlp_session_events(raw).expect("must parse");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].session_id, "019cd1d8-5fa8-7202-bcf6-42b2748dcb88");
        assert_eq!(events[0].event_name, "codex.conversation_starts");
        assert_eq!(events[0].model.as_deref(), Some("gpt-5.4"));
        assert_eq!(events[0].effort.as_deref(), Some("xhigh"));
        assert_eq!(
            events[0]
                .attributes
                .get("conversation.id")
                .map(String::as_str),
            Some("019cd1d8-5fa8-7202-bcf6-42b2748dcb88")
        );
        assert!(!events[0].tool_error);
        assert!(events[1].tool_error);
    }

    #[test]
    fn parse_otlp_session_events_preserves_body_and_extra_attributes() {
        let raw = br#"{
          "resourceLogs":[
            {
              "scopeLogs":[
                {
                  "logRecords":[
                    {
                      "observedTimeUnixNano":"1773047146363148565",
                      "body":{"stringValue":"tool execution failed"},
                      "attributes":[
                        {"key":"event.name","value":{"stringValue":"codex.tool_result"}},
                        {"key":"conversation.id","value":{"stringValue":"019cd1d8-5fa8-7202-bcf6-42b2748dcb88"}},
                        {"key":"tool.name","value":{"stringValue":"exec_command"}},
                        {"key":"error.message","value":{"stringValue":"boom"}}
                      ]
                    }
                  ]
                }
              ]
            }
          ]
        }"#;
        let events = parse_otlp_session_events(raw).expect("must parse");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].body.as_deref(), Some("tool execution failed"));
        assert_eq!(
            events[0].attributes.get("tool.name").map(String::as_str),
            Some("exec_command")
        );
        assert_eq!(
            events[0]
                .attributes
                .get("error.message")
                .map(String::as_str),
            Some("boom")
        );
    }

    #[test]
    fn parse_otlp_session_events_ignores_non_log_payloads() {
        let raw = br#"{
          "resourceSpans":[
            {
              "scopeSpans":[
                {
                  "spans":[
                    {
                      "traceId":"0123456789abcdef0123456789abcdef",
                      "spanId":"0123456789abcdef"
                    }
                  ]
                }
              ]
            }
          ]
        }"#;
        let events = parse_otlp_session_events(raw).expect("must parse");
        assert!(events.is_empty());
    }

    #[test]
    fn apply_session_log_line_tracks_tool_failures_and_context() {
        let mut state = super::FileSessionState::default();
        apply_session_log_line(
            &mut state,
            1_700_000_123,
            "{\"timestamp\":\"2026-03-09T09:05:44.366Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"custom_tool_call\",\"status\":\"failed\"}}",
        )
        .expect("must parse tool call");
        apply_session_log_line(
            &mut state,
            1_700_000_124,
            "{\"timestamp\":\"2026-03-09T09:05:45.366Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"total_tokens\":129200},\"model_context_window\":258400}}}",
        )
        .expect("must parse token count");
        assert_eq!(state.tool_calls, 1);
        assert_eq!(state.tool_errors, 1);
        let left = state.context_left_percent.expect("left percent");
        assert!(left > 49.0 && left < 51.0);
    }

    #[test]
    fn managed_tracker_matching_prefers_session_started_near_record_creation() {
        let record = SessionRecord {
            session_name: "za-codex-za-123".to_string(),
            workspace_root: "/opt/app/za".to_string(),
            workspace_label: "za".to_string(),
            workspace_hash: "hash".to_string(),
            created_at_unix: 1_700_000_000,
            launcher: "up".to_string(),
            launcher_args: Vec::new(),
        };
        let managed_tracker = SessionFileTracker {
            path: PathBuf::from("/tmp/managed.jsonl"),
            offset: 0,
            modified_unix: 1_700_000_020,
            state: FileSessionState {
                session_id: Some("managed-id".to_string()),
                workspace_root: Some("/opt/app/za".to_string()),
                started_unix: Some(1_700_000_010),
                ..FileSessionState::default()
            },
        };
        let direct_tracker = SessionFileTracker {
            path: PathBuf::from("/tmp/direct.jsonl"),
            offset: 0,
            modified_unix: 1_700_010_000,
            state: FileSessionState {
                session_id: Some("direct-id".to_string()),
                workspace_root: Some("/opt/app/za".to_string()),
                started_unix: Some(1_700_010_000),
                ..FileSessionState::default()
            },
        };
        let trackers = vec![&managed_tracker, &direct_tracker];
        let matched = best_tracker_match_for_record(&record, &trackers, &BTreeSet::new())
            .expect("must match tracker");
        assert_eq!(matched, "managed-id");
    }

    #[test]
    fn managed_tracker_matching_prefers_activity_after_record_creation() {
        let record = SessionRecord {
            session_name: "za-codex-za-123".to_string(),
            workspace_root: "/opt/app/za".to_string(),
            workspace_label: "za".to_string(),
            workspace_hash: "hash".to_string(),
            created_at_unix: 1_700_000_300,
            launcher: "up".to_string(),
            launcher_args: Vec::new(),
        };
        let resumed_tracker = SessionFileTracker {
            path: PathBuf::from("/tmp/resumed.jsonl"),
            offset: 0,
            modified_unix: 1_700_000_340,
            state: FileSessionState {
                session_id: Some("resumed-id".to_string()),
                workspace_root: Some("/opt/app/za".to_string()),
                started_unix: Some(1_699_999_000),
                last_activity_unix: Some(1_700_000_340),
                ..FileSessionState::default()
            },
        };
        let previous_tracker = SessionFileTracker {
            path: PathBuf::from("/tmp/previous.jsonl"),
            offset: 0,
            modified_unix: 1_700_000_295,
            state: FileSessionState {
                session_id: Some("previous-id".to_string()),
                workspace_root: Some("/opt/app/za".to_string()),
                started_unix: Some(1_700_000_295),
                last_activity_unix: Some(1_700_000_295),
                ..FileSessionState::default()
            },
        };
        let trackers = vec![&resumed_tracker, &previous_tracker];
        let matched = best_tracker_match_for_record(&record, &trackers, &BTreeSet::new())
            .expect("must match tracker");
        assert_eq!(matched, "resumed-id");
    }

    #[test]
    fn managed_tracker_matching_falls_back_to_most_recent_workspace_tracker() {
        let record = SessionRecord {
            session_name: "za-codex-za-123".to_string(),
            workspace_root: "/opt/app/za".to_string(),
            workspace_label: "za".to_string(),
            workspace_hash: "hash".to_string(),
            created_at_unix: 1_700_100_000,
            launcher: "up".to_string(),
            launcher_args: Vec::new(),
        };
        let older_tracker = SessionFileTracker {
            path: PathBuf::from("/tmp/older.jsonl"),
            offset: 0,
            modified_unix: 1_700_000_100,
            state: FileSessionState {
                session_id: Some("older-id".to_string()),
                workspace_root: Some("/opt/app/za".to_string()),
                last_activity_unix: Some(1_700_000_100),
                ..FileSessionState::default()
            },
        };
        let latest_tracker = SessionFileTracker {
            path: PathBuf::from("/tmp/latest.jsonl"),
            offset: 0,
            modified_unix: 1_700_000_200,
            state: FileSessionState {
                session_id: Some("latest-id".to_string()),
                workspace_root: Some("/opt/app/za".to_string()),
                last_activity_unix: Some(1_700_000_200),
                ..FileSessionState::default()
            },
        };
        let trackers = vec![&older_tracker, &latest_tracker];
        let matched = best_tracker_match_for_record(&record, &trackers, &BTreeSet::new())
            .expect("must match tracker");
        assert_eq!(matched, "latest-id");
    }

    #[test]
    fn build_top_rows_prefers_latest_live_otel_session_for_running_workspace() {
        let tracker = SessionFileTracker {
            path: PathBuf::from("/tmp/older.jsonl"),
            offset: 0,
            modified_unix: 1_700_000_100,
            state: FileSessionState {
                session_id: Some("older-id".to_string()),
                workspace_root: Some("/opt/app/za".to_string()),
                model: Some("gpt-5.4".to_string()),
                effort: Some("xhigh".to_string()),
                last_activity_unix: Some(1_700_000_100),
                ..FileSessionState::default()
            },
        };
        let trackers = BTreeMap::from([(tracker.path.clone(), tracker)]);
        let otel_state = OtelLiveState {
            sessions: BTreeMap::from([(
                "latest-id".to_string(),
                OtelSessionState {
                    model: Some("gpt-5.4".to_string()),
                    effort: Some("xhigh".to_string()),
                    workspace_root: Some("/opt/app/za".to_string()),
                    last_activity_unix: Some(super::current_unix_seconds()),
                    last_event_name: Some("codex.conversation_turn_complete".to_string()),
                    otel_events: 3,
                    api_requests: 1,
                    tool_calls: 0,
                    tool_errors: 0,
                    sse_events: 2,
                },
            )]),
            ..OtelLiveState::default()
        };
        let managed_records = vec![SessionRecord {
            session_name: "za-codex-za-123".to_string(),
            workspace_root: "/opt/app/za".to_string(),
            workspace_label: "za".to_string(),
            workspace_hash: "hash".to_string(),
            created_at_unix: 1_700_000_000,
            launcher: "up".to_string(),
            launcher_args: Vec::new(),
        }];
        let tmux_sessions = BTreeMap::from([(
            "za-codex-za-123".to_string(),
            TmuxSessionInfo {
                created_unix: Some(1_700_000_000),
                activity_unix: Some(super::current_unix_seconds()),
                attached_clients: 1,
            },
        )]);

        let rows = build_top_rows(TopRowsInput {
            current_workspace_root: "/opt/app/za",
            show_all: false,
            show_history: false,
            trackers: &trackers,
            otel_state: &otel_state,
            managed_records: &managed_records,
            tmux_available: true,
            tmux_sessions: &tmux_sessions,
        });

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session_id.as_deref(), Some("latest-id"));
        assert!(rows[0].live_otel);
        assert_eq!(rows[0].otel_events, 3);
    }

    #[test]
    fn rebind_stream_session_switches_to_live_workspace_session_when_current_id_has_no_otel() {
        let now = super::current_unix_seconds();
        let mut app = CodexTopApp::new(PathBuf::from("/opt/app/za"), false, false);
        app.rows = vec![CodexTopRow {
            key: "managed:za-codex-za-123".to_string(),
            session_id: Some("latest-id".to_string()),
            managed_session_name: Some("za-codex-za-123".to_string()),
            workspace_root: "/opt/app/za".to_string(),
            model: Some("gpt-5.4".to_string()),
            effort: Some("xhigh".to_string()),
            context_left_percent: None,
            status: "LIVE".to_string(),
            tmux_running: true,
            attached_clients: 1,
            last_activity_unix: Some(now),
            last_event_name: Some("codex.conversation_turn_complete".to_string()),
            otel_events: 2,
            api_requests: 1,
            live_tool_calls: 0,
            lifetime_tool_calls: 0,
            live_tool_errors: 0,
            lifetime_tool_errors: 0,
            sse_events: 1,
            live_otel: true,
        }];
        app.otel_state.sessions.insert(
            "latest-id".to_string(),
            OtelSessionState {
                model: Some("gpt-5.4".to_string()),
                effort: Some("xhigh".to_string()),
                workspace_root: Some("/opt/app/za".to_string()),
                last_activity_unix: Some(now),
                last_event_name: Some("codex.conversation_turn_complete".to_string()),
                otel_events: 2,
                api_requests: 1,
                tool_calls: 0,
                tool_errors: 0,
                sse_events: 1,
            },
        );
        app.otel_state.session_events.insert(
            "latest-id".to_string(),
            VecDeque::from([
                OtelEventRecord {
                    observed_unix: now.saturating_sub(1),
                    event_name: "codex.conversation_starts".to_string(),
                    tool_error: false,
                    attributes: BTreeMap::new(),
                    body: None,
                },
                OtelEventRecord {
                    observed_unix: now,
                    event_name: "codex.conversation_turn_complete".to_string(),
                    tool_error: false,
                    attributes: BTreeMap::new(),
                    body: Some("done".to_string()),
                },
            ]),
        );
        app.view = TopView::Stream(TopStreamState {
            session_id: "older-id".to_string(),
            workspace_root: "/opt/app/za".to_string(),
            model: Some("gpt-5.4".to_string()),
            effort: Some("xhigh".to_string()),
            tmux_running: false,
            live_otel: false,
            selected: 0,
            scroll_offset: 3,
            viewport_rows: 10,
            follow: true,
        });

        app.rebind_stream_session_if_needed();

        let TopView::Stream(stream) = &app.view else {
            panic!("stream view must remain open");
        };
        assert_eq!(stream.session_id, "latest-id");
        assert!(stream.tmux_running);
        assert!(stream.live_otel);
        assert_eq!(stream.selected, 1);
        assert_eq!(stream.scroll_offset, 0);
        assert_eq!(
            app.status_message.as_deref(),
            Some("stream rebound to live OTel session latest-id")
        );
    }

    #[test]
    fn summarize_codex_session_lines_handles_whitespace_and_reordered_json_fields() {
        let workspaces = BTreeMap::from([("/opt/app/za".to_string(), 1_700_000_000)]);
        let raw = concat!(
            "{\"payload\":{\"cwd\":\"/opt/app/za\",\"id\":\"019cc38e-4d75-7052-b96a-b3a1e36b1868\"},\"type\": \"session_meta\"}\n",
            "{\"payload\":{\"effort\":\"xhigh\",\"model\":\"gpt-5.4\"}, \"type\": \"turn_context\"}\n",
            "{\"type\":\"event_msg\", \"payload\":{\"info\":{\"model_context_window\":258400,\"last_token_usage\":{\"total_tokens\":181807}},\"type\": \"token_count\"}}\n"
        );
        let summary = summarize_codex_session_lines(Cursor::new(raw), 1_700_000_100, &workspaces)
            .expect("must parse")
            .expect("must match workspace");
        assert_eq!(summary.session_id, "019cc38e-4d75-7052-b96a-b3a1e36b1868");
        assert_eq!(summary.model.as_deref(), Some("gpt-5.4"));
        assert_eq!(summary.effort.as_deref(), Some("xhigh"));
        let pct = summary
            .context_left_percent
            .expect("left percent must exist");
        assert!(pct > 29.0 && pct < 30.0);
    }

    #[test]
    fn calculate_context_left_percent_uses_used_tokens_against_context_window() {
        let pct = calculate_context_left_percent(181_807, 258_400).expect("must calculate");
        assert!(pct > 29.0 && pct < 30.0);
    }

    #[test]
    fn parse_legacy_codex_context_left_percent_lines_extracts_percent() {
        let raw = concat!(
            "2026-03-06T14:00:00Z INFO session_loop{thread_id=019cc38e-4d75-7052-b96a-b3a1e36b1868}: codex_core::codex: post sampling token usage turn_id=4 total_usage_tokens=53586 estimated_token_count=Some(51627) auto_compact_limit=244800 token_limit_reached=false needs_follow_up=true\n",
            "2026-03-06T14:00:01Z INFO session_loop{thread_id=ignored-thread}: codex_core::codex: post sampling token usage turn_id=4 total_usage_tokens=1 estimated_token_count=Some(1) auto_compact_limit=10 token_limit_reached=false needs_follow_up=true\n"
        );
        let session_ids = BTreeSet::from(["019cc38e-4d75-7052-b96a-b3a1e36b1868".to_string()]);
        let usage = parse_legacy_codex_context_left_percent_lines(Cursor::new(raw), &session_ids)
            .expect("must parse usage");
        let pct = usage
            .get("019cc38e-4d75-7052-b96a-b3a1e36b1868")
            .copied()
            .expect("usage must exist");
        assert!(pct > 78.0 && pct < 79.0);
    }
}
