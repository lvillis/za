mod analytics;

use anyhow::{Context, Result};
use humantime::format_rfc3339_seconds;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, hash_map::DefaultHasher},
    env, fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    process::Command,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crate::cli::{AiCommands, AiGitCommands, AiGitStatusArgs, AiShell};

const AI_SESSION_KEY: &str = "ZA_AI_SESSION";
const AI_AGENT_KEY: &str = "ZA_AI_AGENT";
const AI_WORKSPACE_KEY: &str = "ZA_AI_WORKSPACE";
const AI_PREV_BASH_ENV_KEY: &str = "ZA_AI_PREV_BASH_ENV";
const AI_BASH_ENV_KEY: &str = "BASH_ENV";
const AI_ANALYTICS_SCHEMA_VERSION: u8 = 1;
const AI_GAIN_SCHEMA_VERSION: u8 = 1;
const TOKEN_ESTIMATE_BYTES_PER_TOKEN: u64 = 4;

const AI_ROUTE_MAP: &[(&str, &str)] = &[
    ("git status", "za ai git status"),
    ("git diff", "za ai git diff"),
];

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct AiDoctorOutput {
    active: bool,
    agent: Option<String>,
    workspace: Option<String>,
    bash_env: Option<String>,
    routes: Vec<AiRoute>,
    issues: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct AiRoute {
    source: String,
    target: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct AiAnalyticsRecord {
    schema_version: u8,
    recorded_at_unix_ms: u64,
    agent: String,
    workspace: String,
    route: String,
    source_command: String,
    raw_bytes: u64,
    summary_bytes: u64,
    raw_estimated_tokens: u64,
    summary_estimated_tokens: u64,
    duration_ms: u64,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum GainView {
    Summary,
    Daily,
    History,
    Graph,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct AiGainOutput {
    schema_version: u8,
    view: GainView,
    days: u64,
    scope: String,
    total_calls: u64,
    total_raw_bytes: u64,
    total_summary_bytes: u64,
    total_saved_bytes: u64,
    total_raw_estimated_tokens: u64,
    total_summary_estimated_tokens: u64,
    total_saved_estimated_tokens: u64,
    total_saved_ratio: f64,
    routes: Vec<AiGainRouteSummary>,
    daily: Vec<AiGainDailySummary>,
    history: Vec<AiGainHistoryEntry>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct AiGainRouteSummary {
    route: String,
    calls: u64,
    raw_bytes: u64,
    summary_bytes: u64,
    saved_bytes: u64,
    raw_estimated_tokens: u64,
    summary_estimated_tokens: u64,
    saved_estimated_tokens: u64,
    saved_ratio: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct AiGainDailySummary {
    day: String,
    calls: u64,
    raw_bytes: u64,
    summary_bytes: u64,
    saved_bytes: u64,
    raw_estimated_tokens: u64,
    summary_estimated_tokens: u64,
    saved_estimated_tokens: u64,
    saved_ratio: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct AiGainHistoryEntry {
    recorded_at: String,
    workspace: String,
    route: String,
    source_command: String,
    raw_bytes: u64,
    summary_bytes: u64,
    saved_bytes: u64,
    raw_estimated_tokens: u64,
    summary_estimated_tokens: u64,
    saved_estimated_tokens: u64,
    saved_ratio: f64,
    duration_ms: u64,
}

pub fn run(cmd: AiCommands) -> Result<i32> {
    match cmd {
        AiCommands::Shell { shell } => {
            print!(
                "{}",
                render_shell_script(shell, &current_session_context()?)
            );
        }
        AiCommands::Env => {
            for (key, value) in current_session_context()?.env_exports() {
                println!("export {key}={}", shell_escape(&value));
            }
        }
        AiCommands::Explain => {
            print!("{}", render_explain());
        }
        AiCommands::Gain {
            days,
            all,
            daily,
            history,
            graph,
            json,
        } => {
            let view = if daily {
                GainView::Daily
            } else if history {
                GainView::History
            } else if graph {
                GainView::Graph
            } else {
                GainView::Summary
            };
            let output = build_gain_output(days, all, view)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&output).context("serialize ai gain output")?
                );
            } else {
                print!("{}", render_gain(&output));
            }
        }
        AiCommands::Doctor { json } => {
            let report = collect_doctor_output()?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).context("serialize ai doctor output")?
                );
            } else {
                print!("{}", render_doctor(&report));
            }
        }
        AiCommands::Git { cmd } => return run_git(cmd),
    }
    Ok(0)
}

pub(crate) fn codex_env_overrides(workspace_root: &Path) -> Result<Vec<(String, String)>> {
    let ctx = AiSessionContext::new("codex", workspace_root)?;
    let bash_env_path = write_codex_bash_env_script(&ctx)?;
    let mut envs = ctx.env_exports();
    if let Some(prev) = env::var_os(AI_BASH_ENV_KEY)
        .map(|value| value.to_string_lossy().trim().to_string())
        .filter(|value| !value.is_empty())
    {
        envs.push((AI_PREV_BASH_ENV_KEY.to_string(), prev));
    }
    envs.push((
        AI_BASH_ENV_KEY.to_string(),
        bash_env_path.display().to_string(),
    ));
    Ok(envs)
}

fn run_git(cmd: AiGitCommands) -> Result<i32> {
    let ctx = current_session_context()?;
    match cmd {
        AiGitCommands::Status { args } => render_and_record_ai_route(
            &ctx,
            "git status",
            vec!["git".to_string(), "status".to_string()],
            || {
                crate::command::diff::render_workspace_output_for_ai(&diff_run_options(
                    args,
                    Vec::new(),
                ))
            },
        ),
        AiGitCommands::Diff { args } => {
            let source_command = raw_git_diff_command(&args);
            let staged = args.staged;
            let common = args.common;
            render_and_record_ai_route(&ctx, "git diff", source_command, || {
                crate::command::diff::render_workspace_output_for_ai(&diff_run_options(
                    common,
                    staged
                        .then_some(crate::command::diff::DiffScope::Staged)
                        .into_iter()
                        .collect(),
                ))
            })
        }
    }
}

fn render_and_record_ai_route(
    ctx: &AiSessionContext,
    route: &str,
    source_command: Vec<String>,
    render: impl FnOnce() -> Result<String>,
) -> Result<i32> {
    let started = Instant::now();
    let summary = render()?;
    let duration_ms = started.elapsed().as_millis() as u64;
    let raw_bytes = capture_command_output_bytes(&source_command).ok();

    print!("{summary}");

    if let Some(raw_bytes) = raw_bytes {
        let summary_bytes = summary.len() as u64;
        let record = AiAnalyticsRecord {
            schema_version: AI_ANALYTICS_SCHEMA_VERSION,
            recorded_at_unix_ms: unix_timestamp_ms(),
            agent: ctx.agent.clone(),
            workspace: ctx.workspace.clone(),
            route: route.to_string(),
            source_command: source_command.join(" "),
            raw_bytes,
            summary_bytes,
            raw_estimated_tokens: estimate_tokens_from_bytes(raw_bytes),
            summary_estimated_tokens: estimate_tokens_from_bytes(summary_bytes),
            duration_ms,
        };
        let _ = analytics::append_record(&record);
    }

    Ok(0)
}

fn capture_command_output_bytes(argv: &[String]) -> Result<u64> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("cannot execute empty source command"))?;
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("capture source command `{}`", argv.join(" ")))?;
    Ok((output.stdout.len() + output.stderr.len()) as u64)
}

fn raw_git_diff_command(args: &crate::cli::AiGitDiffArgs) -> Vec<String> {
    let mut argv = vec!["git".to_string(), "diff".to_string()];
    if args.staged {
        argv.push("--cached".to_string());
    }
    if args.common.name_only {
        argv.push("--name-only".to_string());
    }
    argv
}

fn diff_run_options(
    args: AiGitStatusArgs,
    scopes: Vec<crate::command::diff::DiffScope>,
) -> crate::command::diff::DiffRunOptions {
    crate::command::diff::DiffRunOptions {
        tui: false,
        json: args.json,
        files: args.files,
        name_only: args.name_only,
        path_patterns: args.path,
        scopes,
        kinds: args
            .kind
            .into_iter()
            .map(crate::command::diff::DiffFileKind::from)
            .collect(),
        exclude_risks: args
            .exclude_risk
            .into_iter()
            .map(crate::command::diff::DiffRiskKind::from)
            .collect(),
    }
}

fn current_session_context() -> Result<AiSessionContext> {
    let workspace = env::current_dir().context("read current working directory")?;
    let agent = env::var(AI_AGENT_KEY).unwrap_or_else(|_| "manual".to_string());
    AiSessionContext::new(&agent, &workspace)
}

#[derive(Debug, Clone)]
struct AiSessionContext {
    agent: String,
    workspace: String,
    za_executable: String,
}

impl AiSessionContext {
    fn new(agent: &str, workspace_root: &Path) -> Result<Self> {
        let za_executable = env::current_exe()
            .context("resolve current executable path")?
            .display()
            .to_string();
        Ok(Self {
            agent: agent.to_string(),
            workspace: workspace_root.display().to_string(),
            za_executable,
        })
    }

    fn env_exports(&self) -> Vec<(String, String)> {
        vec![
            (AI_SESSION_KEY.to_string(), "1".to_string()),
            (AI_AGENT_KEY.to_string(), self.agent.clone()),
            (AI_WORKSPACE_KEY.to_string(), self.workspace.clone()),
        ]
    }
}

fn render_shell_script(shell: AiShell, ctx: &AiSessionContext) -> String {
    let mut lines = Vec::new();
    lines.push("# za ai shell".to_string());
    for (key, value) in ctx.env_exports() {
        lines.push(format!("export {key}={}", shell_escape(&value)));
    }
    lines.push(String::new());
    match shell {
        AiShell::Bash | AiShell::Zsh => lines.extend(render_git_wrapper_lines(ctx)),
    }
    lines.join("\n") + "\n"
}

fn render_codex_bash_env_script(ctx: &AiSessionContext) -> String {
    let mut lines = Vec::new();
    lines.push("# za ai codex bash env".to_string());
    lines.push(format!(
        "if [ -n \"${{{AI_PREV_BASH_ENV_KEY}-}}\" ] && [ -r \"${{{AI_PREV_BASH_ENV_KEY}}}\" ] && [ \"${{{AI_PREV_BASH_ENV_KEY}}}\" != \"${{{AI_BASH_ENV_KEY}-}}\" ]; then"
    ));
    lines.push(format!("  . \"${{{AI_PREV_BASH_ENV_KEY}}}\""));
    lines.push("fi".to_string());
    for (key, value) in ctx.env_exports() {
        lines.push(format!("export {key}={}", shell_escape(&value)));
    }
    lines.push(String::new());
    lines.extend(render_git_wrapper_lines(ctx));
    lines.join("\n") + "\n"
}

fn render_git_wrapper_lines(ctx: &AiSessionContext) -> Vec<String> {
    let za = shell_escape(&ctx.za_executable);
    vec![
        "_za_ai_git_diff_should_wrap() {".to_string(),
        "  while [ \"$#\" -gt 0 ]; do".to_string(),
        "    case \"$1\" in".to_string(),
        "      --staged|--cached|--name-only) ;;".to_string(),
        "      *) return 1 ;;".to_string(),
        "    esac".to_string(),
        "    shift".to_string(),
        "  done".to_string(),
        "  return 0".to_string(),
        "}".to_string(),
        "git() {".to_string(),
        "  if [ \"$#\" -eq 0 ]; then".to_string(),
        "    command git".to_string(),
        "    return".to_string(),
        "  fi".to_string(),
        "  case \"$1\" in".to_string(),
        "    status)".to_string(),
        "      shift".to_string(),
        "      if [ \"$#\" -eq 0 ]; then".to_string(),
        format!("        command {za} ai git status"),
        "      else".to_string(),
        "        command git status \"$@\"".to_string(),
        "      fi".to_string(),
        "      ;;".to_string(),
        "    diff)".to_string(),
        "      shift".to_string(),
        "      if _za_ai_git_diff_should_wrap \"$@\"; then".to_string(),
        format!("        command {za} ai git diff \"$@\""),
        "      else".to_string(),
        "        command git diff \"$@\"".to_string(),
        "      fi".to_string(),
        "      ;;".to_string(),
        "    *)".to_string(),
        "      command git \"$@\"".to_string(),
        "      ;;".to_string(),
        "  esac".to_string(),
        "}".to_string(),
    ]
}

fn render_explain() -> String {
    let mut out = String::from("za ai\n\n");
    out.push_str("routes\n");
    for (source, target) in AI_ROUTE_MAP {
        out.push_str(&format!("  {source:<12} {target}\n"));
    }
    out.push_str(
        "\n`za ai shell <shell>` prints session-local git wrappers instead of top-level aliases. In managed Codex sessions, `za run codex` and `za codex` also export `ZA_AI_*` markers and inject a `BASH_ENV` wrapper so bare `git status` and simple `git diff` calls resolve to `za ai git ...` automatically. Unsupported git invocations fall back to raw `git`.\n",
    );
    out.push_str(
        "\nUse `za ai gain` to inspect aggregated raw-vs-summary savings for all AI-routed commands recorded in this workspace.\n",
    );
    out
}

fn collect_doctor_output() -> Result<AiDoctorOutput> {
    let active = env::var_os(AI_SESSION_KEY).is_some();
    let agent = env::var(AI_AGENT_KEY).ok();
    let workspace = env::var(AI_WORKSPACE_KEY).ok();
    let bash_env = env::var(AI_BASH_ENV_KEY).ok();
    let mut issues = Vec::new();
    if active && agent.as_deref().unwrap_or_default().is_empty() {
        issues.push("ZA_AI_AGENT is empty".to_string());
    }
    if active && workspace.as_deref().unwrap_or_default().is_empty() {
        issues.push("ZA_AI_WORKSPACE is empty".to_string());
    }
    if !active {
        issues.push("AI session markers are not active in this shell".to_string());
    }
    if agent.as_deref() == Some("codex") && bash_env.is_none() {
        issues.push("Codex session markers are active but BASH_ENV is not set".to_string());
    }
    Ok(AiDoctorOutput {
        active,
        agent,
        workspace,
        bash_env,
        routes: AI_ROUTE_MAP
            .iter()
            .map(|(source, target)| AiRoute {
                source: (*source).to_string(),
                target: (*target).to_string(),
            })
            .collect(),
        issues,
    })
}

fn render_doctor(report: &AiDoctorOutput) -> String {
    let mut lines = Vec::new();
    lines.push("za ai doctor".to_string());
    lines.push(format!(
        "status    {}",
        if report.active { "active" } else { "inactive" }
    ));
    lines.push(format!(
        "agent     {}",
        report.agent.as_deref().unwrap_or("-")
    ));
    lines.push(format!(
        "workspace {}",
        report.workspace.as_deref().unwrap_or("-")
    ));
    lines.push(format!(
        "bash_env  {}",
        report.bash_env.as_deref().unwrap_or("-")
    ));
    lines.push("routes".to_string());
    for route in &report.routes {
        lines.push(format!("  {:<12} {}", route.source, route.target));
    }
    if !report.issues.is_empty() {
        lines.push("issues".to_string());
        for issue in &report.issues {
            lines.push(format!("  - {issue}"));
        }
    }
    lines.join("\n") + "\n"
}

fn build_gain_output(days: u64, all: bool, view: GainView) -> Result<AiGainOutput> {
    let workspace = env::current_dir()
        .context("read current working directory")?
        .display()
        .to_string();
    let scope = if all {
        "all workspaces".to_string()
    } else {
        workspace.clone()
    };
    let records = analytics::load_records(days, (!all).then_some(workspace.as_str()))?;
    Ok(aggregate_gain_records(days, &scope, view, &records))
}

fn aggregate_gain_records(
    days: u64,
    scope: &str,
    view: GainView,
    records: &[AiAnalyticsRecord],
) -> AiGainOutput {
    let mut routes = BTreeMap::<String, AiGainRouteSummary>::new();
    let mut daily = BTreeMap::<String, AiGainDailySummary>::new();
    let mut history = records
        .iter()
        .map(|record| AiGainHistoryEntry {
            recorded_at: format_unix_ms(record.recorded_at_unix_ms),
            workspace: record.workspace.clone(),
            route: record.route.clone(),
            source_command: record.source_command.clone(),
            raw_bytes: record.raw_bytes,
            summary_bytes: record.summary_bytes,
            saved_bytes: record.raw_bytes.saturating_sub(record.summary_bytes),
            raw_estimated_tokens: record.raw_estimated_tokens,
            summary_estimated_tokens: record.summary_estimated_tokens,
            saved_estimated_tokens: record
                .raw_estimated_tokens
                .saturating_sub(record.summary_estimated_tokens),
            saved_ratio: ratio(
                record.raw_bytes.saturating_sub(record.summary_bytes),
                record.raw_bytes,
            ),
            duration_ms: record.duration_ms,
        })
        .collect::<Vec<_>>();
    let mut total_calls = 0_u64;
    let mut total_raw_bytes = 0_u64;
    let mut total_summary_bytes = 0_u64;
    let mut total_raw_estimated_tokens = 0_u64;
    let mut total_summary_estimated_tokens = 0_u64;

    for record in records {
        total_calls += 1;
        total_raw_bytes += record.raw_bytes;
        total_summary_bytes += record.summary_bytes;
        total_raw_estimated_tokens += record.raw_estimated_tokens;
        total_summary_estimated_tokens += record.summary_estimated_tokens;

        let entry = routes
            .entry(record.route.clone())
            .or_insert_with(|| AiGainRouteSummary {
                route: record.route.clone(),
                calls: 0,
                raw_bytes: 0,
                summary_bytes: 0,
                saved_bytes: 0,
                raw_estimated_tokens: 0,
                summary_estimated_tokens: 0,
                saved_estimated_tokens: 0,
                saved_ratio: 0.0,
            });
        entry.calls += 1;
        entry.raw_bytes += record.raw_bytes;
        entry.summary_bytes += record.summary_bytes;
        entry.raw_estimated_tokens += record.raw_estimated_tokens;
        entry.summary_estimated_tokens += record.summary_estimated_tokens;

        let day_key = unix_ms_day(record.recorded_at_unix_ms);
        let day = daily
            .entry(day_key.clone())
            .or_insert_with(|| AiGainDailySummary {
                day: day_key,
                calls: 0,
                raw_bytes: 0,
                summary_bytes: 0,
                saved_bytes: 0,
                raw_estimated_tokens: 0,
                summary_estimated_tokens: 0,
                saved_estimated_tokens: 0,
                saved_ratio: 0.0,
            });
        day.calls += 1;
        day.raw_bytes += record.raw_bytes;
        day.summary_bytes += record.summary_bytes;
        day.raw_estimated_tokens += record.raw_estimated_tokens;
        day.summary_estimated_tokens += record.summary_estimated_tokens;
    }

    let mut route_rows = routes.into_values().collect::<Vec<_>>();
    for row in &mut route_rows {
        row.saved_bytes = row.raw_bytes.saturating_sub(row.summary_bytes);
        row.saved_estimated_tokens = row
            .raw_estimated_tokens
            .saturating_sub(row.summary_estimated_tokens);
        row.saved_ratio = ratio(row.saved_bytes, row.raw_bytes);
    }
    route_rows.sort_by(|left, right| {
        right
            .saved_estimated_tokens
            .cmp(&left.saved_estimated_tokens)
            .then_with(|| left.route.cmp(&right.route))
    });

    let total_saved_bytes = total_raw_bytes.saturating_sub(total_summary_bytes);
    let total_saved_estimated_tokens =
        total_raw_estimated_tokens.saturating_sub(total_summary_estimated_tokens);

    let mut daily_rows = daily.into_values().collect::<Vec<_>>();
    for row in &mut daily_rows {
        row.saved_bytes = row.raw_bytes.saturating_sub(row.summary_bytes);
        row.saved_estimated_tokens = row
            .raw_estimated_tokens
            .saturating_sub(row.summary_estimated_tokens);
        row.saved_ratio = ratio(row.saved_bytes, row.raw_bytes);
    }
    daily_rows.sort_by(|left, right| left.day.cmp(&right.day));

    history.sort_by(|left, right| right.recorded_at.cmp(&left.recorded_at));
    history.truncate(20);

    AiGainOutput {
        schema_version: AI_GAIN_SCHEMA_VERSION,
        view,
        days,
        scope: scope.to_string(),
        total_calls,
        total_raw_bytes,
        total_summary_bytes,
        total_saved_bytes,
        total_raw_estimated_tokens,
        total_summary_estimated_tokens,
        total_saved_estimated_tokens,
        total_saved_ratio: ratio(total_saved_bytes, total_raw_bytes),
        routes: route_rows,
        daily: daily_rows,
        history,
    }
}

fn render_gain(output: &AiGainOutput) -> String {
    let mut lines = Vec::new();
    lines.push("za ai gain".to_string());
    lines.push(format!("range     {}d", output.days));
    lines.push(format!("scope     {}", output.scope));
    lines.push(format!(
        "calls     {} total  {} routes",
        output.total_calls,
        output.routes.len()
    ));
    lines.push(format!(
        "total     raw {} (~{} tok)  summary {} (~{} tok)  saved {}",
        format_bytes(output.total_raw_bytes),
        output.total_raw_estimated_tokens,
        format_bytes(output.total_summary_bytes),
        output.total_summary_estimated_tokens,
        format_ratio(output.total_saved_ratio)
    ));
    if output.total_calls == 0 {
        lines
            .push("routes    no AI-routed commands recorded in the selected scope yet".to_string());
        return lines.join("\n") + "\n";
    }

    match output.view {
        GainView::Summary => {
            lines.push("routes".to_string());
            for route in &output.routes {
                lines.push(format!(
                    "  {:<12} {:>3} calls  raw {}  summary {}  saved {}",
                    route.route,
                    route.calls,
                    format_bytes(route.raw_bytes),
                    format_bytes(route.summary_bytes),
                    format_ratio(route.saved_ratio)
                ));
            }
        }
        GainView::Daily => {
            lines.push("daily".to_string());
            for day in &output.daily {
                lines.push(format!(
                    "  {:<10} {:>3} calls  raw {}  summary {}  saved {}",
                    day.day,
                    day.calls,
                    format_bytes(day.raw_bytes),
                    format_bytes(day.summary_bytes),
                    format_ratio(day.saved_ratio)
                ));
            }
        }
        GainView::History => {
            lines.push("history".to_string());
            for entry in &output.history {
                lines.push(format!(
                    "  {}  {:<10} saved {}  {}  {}",
                    entry.recorded_at,
                    entry.route,
                    format_ratio(entry.saved_ratio),
                    workspace_label(&entry.workspace),
                    entry.source_command
                ));
            }
        }
        GainView::Graph => {
            lines.push("graph".to_string());
            let max_saved = output
                .daily
                .iter()
                .map(|row| row.saved_estimated_tokens)
                .max()
                .unwrap_or(0);
            for day in &output.daily {
                lines.push(format!(
                    "  {:<10} {:<24} {:>6} tok saved",
                    day.day,
                    graph_bar(day.saved_estimated_tokens, max_saved, 24),
                    day.saved_estimated_tokens
                ));
            }
        }
    }
    lines.join("\n") + "\n"
}

fn write_codex_bash_env_script(ctx: &AiSessionContext) -> Result<PathBuf> {
    let dir = ensure_ai_runtime_dir()?;
    let path = dir.join(format!(
        "codex-{}.bash",
        workspace_script_id(&ctx.workspace)
    ));
    fs::write(&path, render_codex_bash_env_script(ctx))
        .with_context(|| format!("write AI bash env script {}", path.display()))?;
    #[cfg(unix)]
    {
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("set AI bash env script permissions {}", path.display()))?;
    }
    Ok(path)
}

fn ensure_ai_runtime_dir() -> Result<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(runtime_dir) = env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        candidates.push(runtime_dir.join("za-ai"));
    }
    candidates.push(env::temp_dir().join("za-ai"));

    let mut last_error = None;
    for dir in candidates {
        match fs::create_dir_all(&dir) {
            Ok(()) => match ensure_dir_writable(&dir) {
                Ok(()) => return Ok(dir),
                Err(error) => last_error = Some((dir, error)),
            },
            Err(error) => last_error = Some((dir, error.into())),
        }
    }

    let (dir, error) = last_error.expect("AI runtime dir candidates must not be empty");
    Err(error).with_context(|| format!("create AI shell runtime dir {}", dir.display()))
}

fn ensure_dir_writable(dir: &Path) -> Result<()> {
    let probe = dir.join(format!(".za-write-probe-{}", std::process::id()));
    fs::write(&probe, b"ok").with_context(|| format!("write probe file {}", probe.display()))?;
    let _ = fs::remove_file(&probe);
    Ok(())
}

fn estimate_tokens_from_bytes(bytes: u64) -> u64 {
    bytes.div_ceil(TOKEN_ESTIMATE_BYTES_PER_TOKEN)
}

fn ratio(saved: u64, raw: u64) -> f64 {
    if raw == 0 {
        0.0
    } else {
        saved as f64 / raw as f64
    }
}

fn format_ratio(ratio: f64) -> String {
    format!("{:.1}%", ratio * 100.0)
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1}MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1}KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes}B")
    }
}

fn workspace_label(workspace: &str) -> &str {
    workspace.rsplit('/').next().unwrap_or(workspace)
}

fn format_unix_ms(unix_ms: u64) -> String {
    let time = UNIX_EPOCH + std::time::Duration::from_millis(unix_ms);
    format_rfc3339_seconds(time).to_string()
}

fn unix_ms_day(unix_ms: u64) -> String {
    format_unix_ms(unix_ms).chars().take(10).collect()
}

fn graph_bar(value: u64, max_value: u64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if max_value == 0 || value == 0 {
        return " ".repeat(width);
    }
    let filled = ((value as f64 / max_value as f64) * width as f64)
        .round()
        .clamp(1.0, width as f64) as usize;
    format!("{}{}", "█".repeat(filled), " ".repeat(width - filled))
}

fn unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time must be after unix epoch")
        .as_millis() as u64
}

fn workspace_script_id(workspace: &str) -> String {
    let mut hasher = DefaultHasher::new();
    workspace.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
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

#[cfg(test)]
mod tests {
    use super::{
        AI_AGENT_KEY, AI_BASH_ENV_KEY, AI_SESSION_KEY, AI_WORKSPACE_KEY, AiAnalyticsRecord,
        GainView, aggregate_gain_records, codex_env_overrides, estimate_tokens_from_bytes,
        render_explain, render_gain, render_shell_script,
    };
    use crate::cli::AiShell;
    use std::{fs, path::Path};

    #[test]
    fn codex_env_overrides_include_expected_markers_and_bash_env() {
        let envs = codex_env_overrides(Path::new("/opt/app/za")).expect("env overrides");
        assert!(envs.iter().any(|(k, v)| k == AI_SESSION_KEY && v == "1"));
        assert!(envs.iter().any(|(k, v)| k == AI_AGENT_KEY && v == "codex"));
        assert!(
            envs.iter()
                .any(|(k, v)| k == AI_WORKSPACE_KEY && v == "/opt/app/za")
        );
        let bash_env = envs
            .iter()
            .find(|(k, _)| k == AI_BASH_ENV_KEY)
            .map(|(_, v)| v.clone())
            .expect("bash env override");
        assert!(Path::new(&bash_env).exists());
        let script = fs::read_to_string(&bash_env).expect("read bash env script");
        assert!(script.contains("ai git status"));
        assert!(script.contains("ai git diff"));
    }

    #[test]
    fn render_shell_script_emits_git_wrappers_and_exports() {
        let script = render_shell_script(
            AiShell::Bash,
            &super::AiSessionContext {
                agent: "codex".to_string(),
                workspace: "/opt/app/za".to_string(),
                za_executable: "/usr/local/bin/za".to_string(),
            },
        );
        assert!(script.contains("export ZA_AI_SESSION='1'"));
        assert!(script.contains("git() {"));
        assert!(script.contains("command '/usr/local/bin/za' ai git status"));
        assert!(script.contains("command '/usr/local/bin/za' ai git diff \"$@\""));
    }

    #[test]
    fn explain_mentions_ai_git_routes() {
        let out = render_explain();
        assert!(out.contains("git status"));
        assert!(out.contains("za ai git status"));
        assert!(out.contains("za ai gain"));
        assert!(!out.contains("zd"));
    }

    #[test]
    fn aggregate_gain_records_filters_and_sums_routes() {
        let records = vec![
            AiAnalyticsRecord {
                schema_version: 1,
                recorded_at_unix_ms: 1,
                agent: "codex".to_string(),
                workspace: "/opt/app/za".to_string(),
                route: "git status".to_string(),
                source_command: "git status".to_string(),
                raw_bytes: 1200,
                summary_bytes: 300,
                raw_estimated_tokens: 300,
                summary_estimated_tokens: 75,
                duration_ms: 10,
            },
            AiAnalyticsRecord {
                schema_version: 1,
                recorded_at_unix_ms: 2,
                agent: "codex".to_string(),
                workspace: "/opt/app/za".to_string(),
                route: "git diff".to_string(),
                source_command: "git diff --cached".to_string(),
                raw_bytes: 2400,
                summary_bytes: 600,
                raw_estimated_tokens: 600,
                summary_estimated_tokens: 150,
                duration_ms: 20,
            },
        ];

        let out = aggregate_gain_records(7, "/opt/app/za", GainView::Summary, &records);
        assert_eq!(out.total_calls, 2);
        assert_eq!(out.total_raw_bytes, 3600);
        assert_eq!(out.total_saved_bytes, 2700);
        assert_eq!(out.routes.len(), 2);
        assert!(out.total_saved_ratio > 0.7);
        assert_eq!(out.daily.len(), 1);
    }

    #[test]
    fn render_gain_mentions_routes_and_savings() {
        let out = render_gain(&aggregate_gain_records(
            7,
            "/opt/app/za",
            GainView::Summary,
            &[AiAnalyticsRecord {
                schema_version: 1,
                recorded_at_unix_ms: 1,
                agent: "codex".to_string(),
                workspace: "/opt/app/za".to_string(),
                route: "git status".to_string(),
                source_command: "git status".to_string(),
                raw_bytes: 2048,
                summary_bytes: 512,
                raw_estimated_tokens: estimate_tokens_from_bytes(2048),
                summary_estimated_tokens: estimate_tokens_from_bytes(512),
                duration_ms: 15,
            }],
        ));
        assert!(out.contains("za ai gain"));
        assert!(out.contains("git status"));
        assert!(out.contains("saved"));
    }

    #[test]
    fn render_gain_daily_mentions_day_rows() {
        let out = render_gain(&aggregate_gain_records(
            7,
            "all workspaces",
            GainView::Daily,
            &[AiAnalyticsRecord {
                schema_version: 1,
                recorded_at_unix_ms: 1_700_000_000_000,
                agent: "codex".to_string(),
                workspace: "/opt/app/za".to_string(),
                route: "git diff".to_string(),
                source_command: "git diff".to_string(),
                raw_bytes: 4096,
                summary_bytes: 1024,
                raw_estimated_tokens: estimate_tokens_from_bytes(4096),
                summary_estimated_tokens: estimate_tokens_from_bytes(1024),
                duration_ms: 25,
            }],
        ));
        assert!(out.contains("daily"));
        assert!(out.contains("2023-11-14"));
    }
}
