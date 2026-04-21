//! Manage long-lived Codex work sessions backed by tmux.

mod session_state;
mod tmux;
mod top;

use self::session_state::*;
use self::tmux::*;
use self::top::*;
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
        Some(CodexCommands::Ps { json, all }) => run_ps(json, all),
        Some(CodexCommands::Top { all, history }) => run_top(all, history),
        Some(CodexCommands::Stop { json, all }) => run_stop(json, all),
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
            return maybe_attach_or_report(
                &ctx.session_name,
                &ctx.workspace_root,
                &ctx.workspace_label,
            );
        }
    } else {
        start_managed_session(&ctx, CodexLaunchMode::Fresh, launcher, args)?;
    }
    maybe_attach_or_report(&ctx.session_name, &ctx.workspace_root, &ctx.workspace_label)
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
            return maybe_attach_or_report(
                &ctx.session_name,
                &ctx.workspace_root,
                &ctx.workspace_label,
            );
        }
    } else {
        start_managed_session(&ctx, CodexLaunchMode::ResumeLast, launcher, args)?;
    }
    maybe_attach_or_report(&ctx.session_name, &ctx.workspace_root, &ctx.workspace_label)
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
    let command = build_codex_launch_command(&ctx.workspace_root, mode, args)?;
    tmux_new_session(&ctx.session_name, &ctx.workspace_root, &command)?;
    tmux_apply_codex_terminal_fixes(&ctx.session_name)?;
    tmux_apply_codex_session_style(&ctx.session_name, &ctx.workspace_label)?;
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
    maybe_attach_or_report(&ctx.session_name, &ctx.workspace_root, &ctx.workspace_label)
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
        attach_session(&ctx.session_name, &ctx.workspace_label)
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

fn run_ps(json: bool, all: bool) -> Result<i32> {
    let tmux_probe = probe_tmux()?;
    let tmux_available = matches!(tmux_probe, TmuxProbe::Available);
    let tmux_sessions = if tmux_available {
        list_tmux_sessions()?
    } else {
        BTreeMap::new()
    };
    let current_session_name = if all {
        None
    } else {
        Some(resolve_workspace_context()?.session_name)
    };
    let rows = collect_session_rows(
        &tmux_sessions,
        tmux_available,
        current_session_name.as_deref(),
    )?;

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
        println!("{}", no_managed_sessions_message(tmux_available, all));
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

fn run_stop(json: bool, all: bool) -> Result<i32> {
    if all {
        return run_stop_all(json);
    }

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

fn run_stop_all(json: bool) -> Result<i32> {
    let records = load_session_records()?;
    let tmux_probe = probe_tmux()?;
    let tmux_available = matches!(tmux_probe, TmuxProbe::Available);
    let tmux_sessions = if tmux_available {
        list_tmux_sessions()?
    } else {
        BTreeMap::new()
    };
    let record_by_name = records
        .into_iter()
        .map(|record| (record.session_name.clone(), record))
        .collect::<BTreeMap<_, _>>();
    let target_names = stop_target_session_names(&record_by_name, &tmux_sessions);
    if target_names.is_empty() {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&CodexStopAllOutput {
                    tmux_available,
                    sessions: Vec::new(),
                })
                .context("serialize codex stop --all output")?
            );
        } else {
            println!("{}", no_managed_sessions_message(tmux_available, true));
        }
        return Ok(0);
    }

    let mut outputs = Vec::with_capacity(target_names.len());
    for session_name in target_names {
        let record = record_by_name.get(&session_name);
        let metadata_removed = if let Some(record) = record {
            let metadata_path = session_record_metadata_path(record)?;
            let metadata_present = metadata_path.exists();
            remove_session_record(&metadata_path)?;
            metadata_present
        } else {
            false
        };
        let workspace_root = record
            .map(|record| record.workspace_root.clone())
            .unwrap_or_else(|| "<unknown workspace>".to_string());

        let output = if tmux_available {
            let session_running = tmux_sessions.contains_key(&session_name);
            if session_running {
                tmux_kill_session(&session_name)?;
            }
            CodexStopOutput {
                session_name,
                workspace_root,
                stopped: session_running,
                metadata_removed,
                tmux_available: true,
                note: (!session_running).then_some("no running tmux session was found".to_string()),
            }
        } else {
            CodexStopOutput {
                session_name,
                workspace_root,
                stopped: false,
                metadata_removed,
                tmux_available: false,
                note: Some(
                    "`tmux` is not installed; removed local session metadata only".to_string(),
                ),
            }
        };
        outputs.push(output);
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&CodexStopAllOutput {
                tmux_available,
                sessions: outputs,
            })
            .context("serialize codex stop --all output")?
        );
    } else {
        for output in &outputs {
            println!("{}", render_stop_message(output));
        }
    }
    Ok(0)
}

#[derive(Clone, Copy)]
enum CodexLaunchMode {
    Fresh,
    ResumeLast,
}

fn build_codex_launch_command(
    workspace_root: &Path,
    mode: CodexLaunchMode,
    extra_args: &[String],
) -> Result<String> {
    let codex = crate::command::run::resolve_executable_path("codex")?;
    let listener = top_listener_state_for_launch(extra_args)?;
    let mut env_vars = crate::command::run::normalized_proxy_env_from_system()?;
    env_vars.extend(crate::command::ai::codex_env_overrides(workspace_root)?);
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

fn no_managed_sessions_message(tmux_available: bool, all: bool) -> String {
    if all {
        if tmux_available {
            return "No managed Codex sessions found.".to_string();
        }
        return "No managed Codex sessions found. (`tmux` unavailable.)".to_string();
    }

    if tmux_available {
        return "No managed Codex session found for the current workspace. Use `za codex ps --all` to list every local session.".to_string();
    }
    "No managed Codex session found for the current workspace. (`tmux` unavailable.) Use `za codex ps --all` to inspect every locally recorded session.".to_string()
}

fn stop_target_session_names(
    records: &BTreeMap<String, SessionRecord>,
    tmux_sessions: &BTreeMap<String, TmuxSessionInfo>,
) -> Vec<String> {
    let mut names = records.keys().cloned().collect::<BTreeSet<_>>();
    for session_name in tmux_sessions.keys() {
        if session_name.starts_with(SESSION_PREFIX) {
            names.insert(session_name.clone());
        }
    }
    names.into_iter().collect()
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
        CachedSessionSummaryEntry, CodexPsCache, CodexSessionSummary, CodexStopOutput, CodexTopApp,
        CodexTopRow, FileSessionState, LegacyContextCache, OtelEventRecord, OtelLiveState,
        OtelSessionState, SESSION_HASH_LEN, SessionFileTracker, SessionRecord, TmuxSessionInfo,
        TopRowsInput, TopStreamFilter, TopStreamState, TopView, activity_age_label,
        apply_session_log_line, best_tracker_match_for_record, build_shell_exec_command,
        build_top_rows, calculate_context_left_percent, config_overrides_otel,
        ensure_local_listener_no_proxy, is_tmux_no_server, is_tmux_session_absent,
        load_codex_session_summaries, load_legacy_codex_context_left_percent_by_session_id,
        parse_legacy_codex_context_left_percent_lines, parse_otlp_session_events,
        parse_tmux_codex_window_ids, parse_tmux_sessions, render_stop_message, resolve_state_home,
        sanitize_session_label, session_matches_scope, session_status_label, shell_escape,
        stop_target_session_names, summarize_codex_session_lines, tmux_codex_status_left,
        tmux_codex_status_left_length, tmux_panes_include_listener_endpoint,
        tmux_terminal_overrides_disable_alt_screen, workspace_hash,
    };
    use anyhow::Result;
    use std::{
        collections::{BTreeMap, BTreeSet, VecDeque},
        fs,
        io::Cursor,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(name: &str) -> Result<Self> {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time must be after unix epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "za-codex-test-{name}-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path)?;
            Ok(Self { path })
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

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
    fn tmux_codex_status_left_hides_internal_session_name() {
        let left = tmux_codex_status_left("ttd-pro-cli");
        assert_eq!(left, "[ttd-pro-cli] ");
        assert!(!left.contains("za-codex"));
        assert_eq!(tmux_codex_status_left_length("ttd-pro-cli"), left.len());
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
    fn session_matches_scope_filters_to_current_workspace_session() {
        assert!(session_matches_scope(
            "za-codex-current-123",
            Some("za-codex-current-123")
        ));
        assert!(!session_matches_scope(
            "za-codex-other-456",
            Some("za-codex-current-123")
        ));
        assert!(session_matches_scope("za-codex-other-456", None));
    }

    #[test]
    fn stop_target_session_names_merges_records_and_managed_tmux_sessions() {
        let records = BTreeMap::from([(
            "za-codex-current-123".to_string(),
            SessionRecord {
                session_name: "za-codex-current-123".to_string(),
                workspace_root: "/opt/app/current".to_string(),
                workspace_label: "current".to_string(),
                workspace_hash: "hash-current".to_string(),
                created_at_unix: 1_700_000_000,
                launcher: "up".to_string(),
                launcher_args: Vec::new(),
            },
        )]);
        let tmux_sessions = BTreeMap::from([
            (
                "za-codex-current-123".to_string(),
                TmuxSessionInfo {
                    created_unix: Some(1_700_000_000),
                    activity_unix: Some(1_700_000_010),
                    attached_clients: 1,
                },
            ),
            (
                "za-codex-orphan-456".to_string(),
                TmuxSessionInfo {
                    created_unix: Some(1_700_000_001),
                    activity_unix: Some(1_700_000_020),
                    attached_clients: 0,
                },
            ),
            (
                "other-session".to_string(),
                TmuxSessionInfo {
                    created_unix: Some(1_700_000_002),
                    activity_unix: Some(1_700_000_030),
                    attached_clients: 0,
                },
            ),
        ]);

        let names = stop_target_session_names(&records, &tmux_sessions);

        assert_eq!(
            names,
            vec![
                "za-codex-current-123".to_string(),
                "za-codex-orphan-456".to_string()
            ]
        );
    }

    #[test]
    fn load_codex_session_summaries_reuses_cached_file_summary() {
        let dir = TempDir::new("summary-cache").expect("temp dir");
        let sessions_root = dir.path.join("codex/sessions/2026/04/21");
        fs::create_dir_all(&sessions_root).expect("create sessions root");
        let session_path = sessions_root.join("session.jsonl");
        fs::write(&session_path, "not-json\n").expect("write placeholder session log");
        let metadata = fs::metadata(&session_path).expect("session metadata");
        let modified_unix = metadata
            .modified()
            .expect("mtime")
            .duration_since(UNIX_EPOCH)
            .expect("mtime after epoch")
            .as_secs();

        let mut cache = CodexPsCache {
            session_files: BTreeMap::from([(
                session_path.display().to_string(),
                CachedSessionSummaryEntry {
                    len: metadata.len(),
                    modified_unix,
                    summary: Some(CodexSessionSummary {
                        session_id: "cached-session".to_string(),
                        workspace_root: "/opt/app/za".to_string(),
                        modified_unix,
                        model: Some("gpt-5.4".to_string()),
                        effort: Some("xhigh".to_string()),
                        context_left_percent: Some(42.0),
                    }),
                },
            )]),
            ..CodexPsCache::default()
        };
        let records = vec![SessionRecord {
            session_name: "za-codex-za-123".to_string(),
            workspace_root: "/opt/app/za".to_string(),
            workspace_label: "za".to_string(),
            workspace_hash: "hash".to_string(),
            created_at_unix: 1_700_000_000,
            launcher: "up".to_string(),
            launcher_args: Vec::new(),
        }];

        let summaries =
            load_codex_session_summaries(&records, &dir.path.join("codex/sessions"), &mut cache)
                .expect("must load summaries");

        let summary = summaries.get("/opt/app/za").expect("cached summary");
        assert_eq!(summary.session_id, "cached-session");
        assert_eq!(summary.model.as_deref(), Some("gpt-5.4"));
        assert_eq!(summary.effort.as_deref(), Some("xhigh"));
        assert_eq!(summary.context_left_percent, Some(42.0));
    }

    #[test]
    fn load_legacy_context_reuses_cached_log_usage() {
        let dir = TempDir::new("legacy-cache").expect("temp dir");
        let log_path = dir.path.join("codex/log/codex-tui.log");
        fs::create_dir_all(log_path.parent().expect("log parent")).expect("create log dir");
        fs::write(&log_path, "garbage\n").expect("write placeholder log");
        let metadata = fs::metadata(&log_path).expect("log metadata");
        let modified_unix = metadata
            .modified()
            .expect("mtime")
            .duration_since(UNIX_EPOCH)
            .expect("mtime after epoch")
            .as_secs();
        let session_ids = BTreeSet::from(["cached-session".to_string()]);
        let mut cache = CodexPsCache {
            legacy_log: Some(LegacyContextCache {
                len: metadata.len(),
                modified_unix,
                values: BTreeMap::from([
                    ("cached-session".to_string(), 58.5),
                    ("other-session".to_string(), 12.0),
                ]),
            }),
            ..CodexPsCache::default()
        };

        let usage = load_legacy_codex_context_left_percent_by_session_id(
            &session_ids,
            &log_path,
            &mut cache,
        )
        .expect("must load legacy usage");

        assert_eq!(usage.get("cached-session").copied(), Some(58.5));
        assert!(!usage.contains_key("other-session"));
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
            status: "connected".to_string(),
            tmux_running: true,
            attached_clients: 1,
            last_activity_unix: Some(now),
            otel_last_activity_unix: Some(now),
            last_event_name: Some("codex.conversation_turn_complete".to_string()),
            otel_events: 2,
            api_requests: 1,
            live_tool_calls: 0,
            lifetime_tool_calls: 0,
            live_tool_errors: 0,
            lifetime_tool_errors: 0,
            sse_events: 1,
            live_otel: true,
            status_detail: "tmux active and live OTel is flowing".to_string(),
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
            detail_scroll_offset: 0,
            detail_viewport_rows: 6,
            follow: true,
            filter: TopStreamFilter::All,
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
