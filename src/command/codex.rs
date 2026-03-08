//! Manage long-lived Codex work sessions backed by tmux.

use crate::cli::CodexCommands;
use anyhow::{Context, Result, anyhow, bail};
use ignore::WalkBuilder;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::{self, BufRead, BufReader, ErrorKind, IsTerminal},
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

const SESSION_PREFIX: &str = "za-codex";
const STATE_DIR_RELATIVE: &str = "za/codex/sessions";
const DEFAULT_WORKSPACE_LABEL: &str = "workspace";
const SESSION_HASH_LEN: usize = 12;
const SESSION_LABEL_MAX_LEN: usize = 24;

pub fn run(cmd: Option<CodexCommands>, passthrough_args: &[String]) -> Result<i32> {
    match cmd {
        Some(CodexCommands::Up { args }) => run_up(&args),
        Some(CodexCommands::Attach) => run_attach(),
        Some(CodexCommands::Exec { args }) => run_exec(&args),
        Some(CodexCommands::Resume { args }) => run_resume(&args),
        Some(CodexCommands::Ps { json }) => run_ps(json),
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
    if tmux_has_session(&ctx.session_name)? {
        if force_recreate {
            restart_managed_session(&ctx, launcher, args)?;
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
    if tmux_has_session(&ctx.session_name)? {
        if force_recreate {
            restart_managed_resume_session(&ctx, launcher, args)?;
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
struct SessionMetaEvent {
    payload: SessionMetaPayload,
}

#[derive(Debug, Deserialize)]
struct SessionMetaPayload {
    id: String,
    cwd: String,
}

#[derive(Debug, Deserialize)]
struct TurnContextEvent {
    payload: TurnContextPayload,
}

#[derive(Debug, Deserialize)]
struct TurnContextPayload {
    model: Option<String>,
    effort: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenCountEvent {
    payload: TokenCountPayload,
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
struct TokenUsage {
    total_tokens: u64,
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
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("cannot resolve state directory: set `HOME`"))?;
    Ok(env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/state")))
}

fn build_codex_launch_command(mode: CodexLaunchMode, extra_args: &[String]) -> Result<String> {
    let codex = crate::command::run::resolve_executable_path("codex")?;
    let mut argv = Vec::new();
    argv.push(codex.display().to_string());
    argv.push("--no-alt-screen".to_string());
    if matches!(mode, CodexLaunchMode::ResumeLast) {
        argv.push("resume".to_string());
        argv.push("--last".to_string());
    }
    argv.extend(extra_args.iter().cloned());
    build_shell_exec_command(
        &crate::command::run::normalized_proxy_env_from_system()?,
        &argv,
    )
}

fn build_exec_command(args: &[String]) -> Result<String> {
    build_shell_exec_command(
        &crate::command::run::normalized_proxy_env_from_system()?,
        args,
    )
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
    if is_tmux_missing_session(&stderr) || is_tmux_no_server(&stderr) {
        return Ok(false);
    }
    bail!(
        "`tmux has-session -t {session_name}` failed: {}",
        stderr.trim()
    )
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
    if let Some(path) = env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(path));
    }
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("cannot resolve Codex home: set `HOME`"))?;
    Ok(home.join(".codex"))
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
        if session_id.is_none() && line.contains("\"type\":\"session_meta\"") {
            let event = match serde_json::from_str::<SessionMetaEvent>(&line) {
                Ok(event) => event,
                Err(_) => continue,
            };
            let cwd = event.payload.cwd.trim();
            let Some(started_unix) = workspace_starts.get(cwd) else {
                return Ok(None);
            };
            if modified_unix + 300 < *started_unix {
                return Ok(None);
            }
            session_id = Some(event.payload.id);
            workspace_root = Some(cwd.to_string());
            continue;
        }

        if session_id.is_some()
            && line.contains("\"type\":\"turn_context\"")
            && let Ok(event) = serde_json::from_str::<TurnContextEvent>(&line)
        {
            if let Some(value) = event
                .payload
                .model
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
            {
                model = Some(value);
            }
            if let Some(value) = event
                .payload
                .effort
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
            {
                effort = Some(value);
            }
        }

        if session_id.is_some()
            && line.contains("\"type\":\"token_count\"")
            && let Ok(event) = serde_json::from_str::<TokenCountEvent>(&line)
        {
            context_left_percent = calculate_context_left_percent(
                event.payload.info.last_token_usage.total_tokens,
                event.payload.info.model_context_window,
            );
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
    lower.contains("failed to connect to server") || lower.contains("no server running")
}

fn is_tmux_missing_session(stderr: &str) -> bool {
    stderr
        .trim()
        .to_ascii_lowercase()
        .contains("can't find session")
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
        CodexStopOutput, SESSION_HASH_LEN, activity_age_label, build_shell_exec_command,
        calculate_context_left_percent, parse_legacy_codex_context_left_percent_lines,
        parse_tmux_sessions, render_stop_message, sanitize_session_label, session_status_label,
        shell_escape, summarize_codex_session_lines, workspace_hash,
    };
    use std::{
        collections::{BTreeMap, BTreeSet},
        io::Cursor,
        path::Path,
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
