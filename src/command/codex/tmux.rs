use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TmuxProbe {
    Available,
    Missing,
}

#[derive(Clone, Debug)]
pub(super) struct TmuxSessionInfo {
    pub(super) created_unix: Option<u64>,
    pub(super) activity_unix: Option<u64>,
    pub(super) attached_clients: usize,
}

pub(super) fn ensure_tmux_available() -> Result<()> {
    match probe_tmux()? {
        TmuxProbe::Available => Ok(()),
        TmuxProbe::Missing => bail!("`za codex` requires `tmux`; install it first"),
    }
}

pub(super) fn probe_tmux() -> Result<TmuxProbe> {
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

pub(super) fn tmux_has_session(session_name: &str) -> Result<bool> {
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

pub(super) fn tmux_session_needs_top_listener_restart(
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

pub(super) fn tmux_apply_codex_terminal_fixes(session_name: &str) -> Result<()> {
    tmux_ensure_outer_scrollback_preserved()?;
    tmux_disable_alternate_screen_for_codex_windows(session_name)?;
    Ok(())
}

pub(super) fn tmux_apply_codex_session_style(
    session_name: &str,
    workspace_label: &str,
) -> Result<()> {
    tmux_set_session_option(
        session_name,
        "status-left",
        &tmux_codex_status_left(workspace_label),
    )?;
    tmux_set_session_option(
        session_name,
        "status-left-length",
        &tmux_codex_status_left_length(workspace_label).to_string(),
    )?;

    if let Some(window_id) = tmux_primary_codex_window_id(session_name)? {
        tmux_rename_window(&window_id, "main")?;
        tmux_set_window_option(&window_id, "automatic-rename", "off")?;
    }

    Ok(())
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

pub(super) fn parse_tmux_codex_window_ids(output: &str) -> BTreeSet<String> {
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

pub(super) fn tmux_terminal_overrides_disable_alt_screen(output: &str) -> bool {
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

fn tmux_set_session_option(target: &str, option: &str, value: &str) -> Result<()> {
    let output = Command::new("tmux")
        .args(["set-option", "-t", target, option, value])
        .output()
        .with_context(|| format!("set tmux session option `{option}` for `{target}`"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "`tmux set-option -t {target} {option} {value}` failed: {}",
            stderr.trim()
        );
    }
    Ok(())
}

fn tmux_rename_window(target: &str, window_name: &str) -> Result<()> {
    let output = Command::new("tmux")
        .args(["rename-window", "-t", target, window_name])
        .output()
        .with_context(|| format!("rename tmux window `{target}` to `{window_name}`"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "`tmux rename-window -t {target} {window_name}` failed: {}",
            stderr.trim()
        );
    }
    Ok(())
}

fn tmux_primary_codex_window_id(session_name: &str) -> Result<Option<String>> {
    if let Some(window_id) = tmux_codex_window_ids(session_name)?.into_iter().next() {
        return Ok(Some(window_id));
    }
    tmux_first_window_id(session_name)
}

fn tmux_first_window_id(session_name: &str) -> Result<Option<String>> {
    let output = Command::new("tmux")
        .args(["list-windows", "-t", session_name, "-F", "#{window_id}"])
        .output()
        .with_context(|| format!("list tmux windows for `{session_name}`"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if is_tmux_session_absent(&stderr) {
            return Ok(None);
        }
        bail!(
            "`tmux list-windows -t {session_name}` failed: {}",
            stderr.trim()
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned))
}

pub(super) fn tmux_panes_include_listener_endpoint(output: &str, endpoint: &str) -> bool {
    output.lines().any(|line| {
        let Some((current_command, start_command)) = line.split_once('\t') else {
            return false;
        };
        current_command.trim() == "codex" && start_command.contains(endpoint)
    })
}

pub(super) fn tmux_new_session(session_name: &str, cwd: &Path, command: &str) -> Result<()> {
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

pub(super) fn tmux_new_window(
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

pub(super) fn tmux_kill_session(session_name: &str) -> Result<()> {
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

pub(super) fn maybe_attach_or_report(
    session_name: &str,
    workspace_root: &Path,
    workspace_label: &str,
) -> Result<i32> {
    if is_interactive_terminal() {
        return attach_session(session_name, workspace_label);
    }

    println!(
        "Codex session `{}` is ready for {}.",
        session_name,
        workspace_root.display()
    );
    Ok(0)
}

pub(super) fn attach_session(session_name: &str, workspace_label: &str) -> Result<i32> {
    tmux_apply_codex_terminal_fixes(session_name)?;
    tmux_apply_codex_session_style(session_name, workspace_label)?;
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

pub(super) fn tmux_codex_status_left(workspace_label: &str) -> String {
    format!("[{workspace_label}] ")
}

pub(super) fn tmux_codex_status_left_length(workspace_label: &str) -> usize {
    tmux_codex_status_left(workspace_label).len()
}

pub(super) fn is_interactive_terminal() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

pub(super) fn list_tmux_sessions() -> Result<BTreeMap<String, TmuxSessionInfo>> {
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

pub(super) fn parse_tmux_sessions(raw: &str) -> Result<BTreeMap<String, TmuxSessionInfo>> {
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

pub(super) fn is_tmux_no_server(stderr: &str) -> bool {
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

pub(super) fn is_tmux_session_absent(stderr: &str) -> bool {
    is_tmux_missing_session(stderr) || is_tmux_no_server(stderr)
}
