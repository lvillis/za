use super::*;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

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

const TMUX_CODEX_STATUS_INTERVAL_SECS: &str = "5";
const TMUX_CODEX_LOC_CACHE_TTL_SECS: &str = "60";
const TMUX_CODEX_STATUS_DIR: &str = "codex-status";
const TMUX_CODEX_STATUS_WIDTH: usize = 72;
const TMUX_CODEX_PRIMARY_WINDOW_NAME: &str = "work";
const TMUX_CODEX_SERVER_BOOTSTRAP_PREFIX: &str = "codex-server-bootstrap";
const TMUX_CODEX_SERVER_BOOTSTRAP_COMMAND: &str = "sleep 60";
const TMUX_CODEX_STATUS_HELPER_SCRIPT: &str = r#"#!/bin/sh
root=$1
cache=$2
ttl=${3:-60}

count_diff() {
  git -C "$root" diff --numstat HEAD -- . 2>/dev/null |
    awk 'BEGIN { a = 0; d = 0 } $1 ~ /^[0-9]+$/ { a += $1 } $2 ~ /^[0-9]+$/ { d += $2 } END { printf "+%d -%d", a, d }'
}

count_lines() {
  git -C "$root" ls-files -co --exclude-standard -z -- "$@" 2>/dev/null |
    xargs -0 -r wc -l 2>/dev/null |
    awk '$1 ~ /^[0-9]+$/ && $2 != "total" { s += $1 } END { print s + 0 }'
}

count_tokei_loc() {
  command -v tokei >/dev/null 2>&1 || return 1
  command -v jq >/dev/null 2>&1 || return 1
  tokei --output json "$root" 2>/dev/null | jq -r '
    . as $data
    | def code($name): ($data[$name].code // 0);
      [
        code("Total"),
        code("Rust"),
        (
          [
            "Astro",
            "CSS",
            "HTML",
            "JSX",
            "JavaScript",
            "LESS",
            "Sass",
            "Svelte",
            "SVG",
            "TSX",
            "TypeScript",
            "Vue"
          ] | map(code(.)) | add
        )
      ] | @tsv
  ' 2>/dev/null
}

cargo_metadata_value() {
  key=$1
  manifest=$root/Cargo.toml
  [ -r "$manifest" ] || return 1
  awk -v key="$key" '
    function trim(s) {
      sub(/^[[:space:]]+/, "", s)
      sub(/[[:space:]]+$/, "", s)
      return s
    }
    function unquote(s) {
      s = trim(s)
      if (s ~ /^"/) {
        sub(/^"/, "", s)
        sub(/".*$/, "", s)
      }
      return s
    }
    /^[[:space:]]*#/ {
      next
    }
    /^[[:space:]]*\[/ {
      section = $0
      sub(/^[[:space:]]*\[/, "", section)
      sub(/\][[:space:]]*$/, "", section)
      next
    }
    {
      pattern = "^[[:space:]]*" key "[[:space:]]*="
      if ($0 !~ pattern) {
        next
      }
      value = $0
      sub(/^[^=]*=/, "", value)
      value = unquote(value)
      if (section == "package") {
        print value
        found = 1
        exit
      }
      if (section == "workspace.package" && fallback == "") {
        fallback = value
      }
    }
    END {
      if (!found && fallback != "") {
        print fallback
      }
    }
  ' "$manifest"
}

normalize_msrv() {
  version=$1
  case "$version" in
    *.*.0) version=${version%.0} ;;
  esac
  printf '%s' "$version"
}

build_rust_metadata() {
  pkg_version=$(cargo_metadata_value version 2>/dev/null || true)
  msrv=$(cargo_metadata_value rust-version 2>/dev/null || true)
  out=
  if [ -n "$pkg_version" ]; then
    out="$pkg_version"
  fi
  if [ -n "$msrv" ]; then
    msrv=$(normalize_msrv "$msrv")
    if [ -n "$out" ]; then
      out="$out | msrv $msrv"
    else
      out="msrv $msrv"
    fi
  fi
  [ -n "$out" ] && printf '%s  ' "$out"
}

compact_count() {
  awk -v n="${1:-0}" 'BEGIN {
    n += 0
    if (n >= 1000000) {
      if (n % 1000000 == 0) printf "%dm", n / 1000000
      else printf "%.1fm", n / 1000000
    } else if (n >= 1000) {
      if (n % 1000 == 0) printf "%dk", n / 1000
      else printf "%.1fk", n / 1000
    } else {
      printf "%d", n
    }
  }'
}

read_cached_loc() {
  [ -n "$cache" ] && [ -r "$cache" ] || return 1
  IFS=' ' read -r cached_at all rust web < "$cache" || return 1
  case "$cached_at" in ''|*[!0-9]*) return 1 ;; esac
  case "$all" in ''|*[!0-9]*) return 1 ;; esac
  case "$rust" in ''|*[!0-9]*) return 1 ;; esac
  case "$web" in ''|*[!0-9]*) return 1 ;; esac
  age=$((now - cached_at))
  [ "$age" -ge 0 ] 2>/dev/null || return 1
  [ "$age" -lt "$ttl" ] 2>/dev/null || return 1
  return 0
}

write_loc_cache() {
  [ -n "$cache" ] || return 0
  dir=${cache%/*}
  [ "$dir" != "$cache" ] || return 0
  mkdir -p "$dir" 2>/dev/null || return 0
  tmp="$cache.$$"
  printf '%s %s %s %s\n' "$now" "$all" "$rust" "$web" > "$tmp" 2>/dev/null &&
    mv -f "$tmp" "$cache" 2>/dev/null
}

compute_fallback_loc() {
  all=$(count_lines \
    '*.rs' '*.c' '*.h' '*.hpp' '*.cpp' '*.cc' '*.cxx' \
    '*.go' '*.py' '*.java' '*.kt' '*.kts' '*.swift' '*.scala' \
    '*.rb' '*.php' '*.ex' '*.exs' '*.erl' '*.hrl' \
    '*.js' '*.jsx' '*.ts' '*.tsx' '*.mjs' '*.cjs' '*.mts' '*.cts' \
    '*.vue' '*.svelte' '*.astro' '*.css' '*.scss' '*.sass' '*.less' '*.html' \
    '*.sh' '*.bash' '*.zsh' '*.fish' '*.sql' '*.proto')
  rust=$(count_lines '*.rs')
  web=$(count_lines \
    '*.js' '*.jsx' '*.ts' '*.tsx' '*.mjs' '*.cjs' '*.mts' '*.cts' \
    '*.vue' '*.svelte' '*.astro' '*.css' '*.scss' '*.sass' '*.less' '*.html' \
    '*.svg')
}

compute_loc() {
  if loc=$(count_tokei_loc); then
    set -- $loc
    all=${1:-0}
    rust=${2:-0}
    web=${3:-0}
  else
    compute_fallback_loc
  fi
  write_loc_cache
}

now=$(date +%s 2>/dev/null || printf '0')
case "$ttl" in ''|*[!0-9]*) ttl=60 ;; esac

metadata=$(build_rust_metadata)
diff=$(count_diff)

if ! read_cached_loc; then
  compute_loc
fi

printf '%s[diff %s]  loc %s | rust %s | web %s\n' "$metadata" "$diff" "$(compact_count "$all")" "$(compact_count "$rust")" "$(compact_count "$web")"
"#;

#[derive(Clone, Debug)]
struct TmuxCodexStatusHelper {
    script_path: PathBuf,
    cache_path: PathBuf,
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
        "Could not inspect Codex session `{session_name}`: {}",
        tmux_failure_detail(&stderr)
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
            "Could not inspect Codex session panes for `{session_name}`: {}",
            tmux_failure_detail(&stderr)
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
    workspace_root: &Path,
) -> Result<()> {
    let status_helper = ensure_tmux_codex_status_helper(workspace_root)?;
    tmux_set_session_option(
        session_name,
        "status-left",
        &tmux_codex_status_left_for_helper(workspace_label, workspace_root, &status_helper),
    )?;
    tmux_set_session_option(
        session_name,
        "status-left-length",
        &tmux_codex_status_left_length()?.to_string(),
    )?;
    tmux_set_session_option(
        session_name,
        "status-interval",
        TMUX_CODEX_STATUS_INTERVAL_SECS,
    )?;
    tmux_set_session_option(session_name, "status-format[0]", tmux_codex_status_format())?;

    if let Some(window_id) = tmux_primary_codex_window_id(session_name)? {
        tmux_rename_window(&window_id, TMUX_CODEX_PRIMARY_WINDOW_NAME)?;
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
            "Could not configure tmux scrollback preservation: {}",
            tmux_failure_detail(&stderr)
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
        bail!(
            "Could not read tmux server option `{option}`: {}",
            tmux_failure_detail(&stderr)
        );
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
            "Could not inspect Codex session panes for `{session_name}`: {}",
            tmux_failure_detail(&stderr)
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
            "Could not configure Codex window `{target}` option `{option}`: {}",
            tmux_failure_detail(&stderr)
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
            "Could not configure Codex session `{target}` option `{option}`: {}",
            tmux_failure_detail(&stderr)
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
            "Could not rename Codex window `{target}` to `{window_name}`: {}",
            tmux_failure_detail(&stderr)
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
            "Could not inspect Codex session windows for `{session_name}`: {}",
            tmux_failure_detail(&stderr)
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
    let bootstrap_session = tmux_start_neutral_server_if_needed()?;
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
        tmux_cleanup_bootstrap_session(bootstrap_session.as_deref());
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = tmux_kill_server_if_empty();
        bail!(
            "Could not start Codex session `{session_name}`: {}",
            tmux_failure_detail(&stderr)
        );
    }
    tmux_cleanup_bootstrap_session(bootstrap_session.as_deref());
    Ok(())
}

fn tmux_start_neutral_server_if_needed() -> Result<Option<String>> {
    if !list_tmux_sessions()?.is_empty() {
        return Ok(None);
    }

    tmux_kill_server_if_empty()?;

    let session_name = tmux_neutral_server_bootstrap_session_name();
    let output = Command::new("tmux")
        .arg("new-session")
        .arg("-d")
        .arg("-s")
        .arg(&session_name)
        .arg("-c")
        .arg(env::temp_dir())
        .arg(TMUX_CODEX_SERVER_BOOTSTRAP_COMMAND)
        .output()
        .context("start neutral tmux server")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Could not start neutral tmux server: {}",
            tmux_failure_detail(&stderr)
        );
    }
    Ok(Some(session_name))
}

fn tmux_neutral_server_bootstrap_session_name() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    format!(
        "{}-{}-{now}",
        TMUX_CODEX_SERVER_BOOTSTRAP_PREFIX,
        process::id()
    )
}

fn tmux_cleanup_bootstrap_session(session_name: Option<&str>) {
    if let Some(session_name) = session_name {
        let _ = tmux_kill_session(session_name);
    }
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
            "Could not open command window in Codex session `{session_name}`: {}",
            tmux_failure_detail(&stderr)
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
            "Could not stop Codex session `{session_name}`: {}",
            tmux_failure_detail(&stderr)
        );
    }
    Ok(())
}

pub(super) fn tmux_kill_server_if_empty() -> Result<bool> {
    if !list_tmux_sessions()?.is_empty() {
        return Ok(false);
    }

    let output = Command::new("tmux")
        .arg("kill-server")
        .output()
        .context("stop empty tmux server")?;
    if output.status.success() {
        return Ok(true);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if is_tmux_no_server(&stderr) {
        return Ok(false);
    }
    bail!(
        "Could not stop empty tmux server: {}",
        tmux_failure_detail(&stderr)
    )
}

pub(super) fn maybe_attach_or_report(
    session_name: &str,
    workspace_root: &Path,
    workspace_label: &str,
) -> Result<i32> {
    if is_interactive_terminal() {
        return attach_session(session_name, workspace_label, workspace_root);
    }

    tmux_apply_codex_terminal_fixes(session_name)?;
    tmux_apply_codex_session_style(session_name, workspace_label, workspace_root)?;
    println!(
        "Codex session `{}` is ready for {}.",
        session_name,
        workspace_root.display()
    );
    Ok(0)
}

pub(super) fn attach_session(
    session_name: &str,
    workspace_label: &str,
    workspace_root: &Path,
) -> Result<i32> {
    tmux_apply_codex_terminal_fixes(session_name)?;
    tmux_apply_codex_session_style(session_name, workspace_label, workspace_root)?;
    if tmux_current_session_is(session_name)? {
        return Ok(0);
    }

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

pub(super) fn tmux_current_session_is(session_name: &str) -> Result<bool> {
    Ok(tmux_current_session_name()?.as_deref() == Some(session_name))
}

fn tmux_current_session_name() -> Result<Option<String>> {
    if env::var_os("TMUX").is_none() {
        return Ok(None);
    }

    let output = Command::new("tmux")
        .args(["display-message", "-p", "#{session_name}"])
        .output()
        .context("inspect current tmux session")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if is_tmux_no_server(&stderr) {
            return Ok(None);
        }
        bail!(
            "Could not inspect current tmux session: {}",
            tmux_failure_detail(&stderr)
        );
    }

    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok((!name.is_empty()).then_some(name))
}

#[cfg(test)]
pub(super) fn tmux_codex_status_left(
    workspace_label: &str,
    workspace_root: &Path,
) -> Result<String> {
    let helper = tmux_codex_status_helper_paths(workspace_root)?;
    Ok(tmux_codex_status_left_for_helper(
        workspace_label,
        workspace_root,
        &helper,
    ))
}

fn tmux_codex_status_left_for_helper(
    _workspace_label: &str,
    workspace_root: &Path,
    helper: &TmuxCodexStatusHelper,
) -> String {
    format!("{} ", tmux_codex_status_for_helper(workspace_root, helper))
}

pub(super) fn tmux_codex_status_left_length() -> Result<usize> {
    Ok(TMUX_CODEX_STATUS_WIDTH)
}

pub(super) fn tmux_codex_status_format() -> &'static str {
    concat!(
        "#[align=left range=left #{E:status-left-style}]",
        "#[push-default]#{T;=/#{status-left-length}:status-left}#[pop-default]",
        "#{?#{>:#{session_windows},1},",
        "#[list=on align=#{status-justify}]",
        "#{W:#[push-default]#{T:window-status-format}#[pop-default]#{?window_end_flag,,#{window-status-separator}},",
        "#[push-default]#{T:window-status-current-format}#[pop-default]#{?window_end_flag,,#{window-status-separator}}}",
        "#[nolist],",
        "}",
        "#[align=right range=right #{E:status-right-style}]",
        "#[push-default]#{T;=/#{status-right-length}:status-right}#[pop-default]",
        "#[norange default]",
    )
}

fn tmux_codex_status_for_helper(workspace_root: &Path, helper: &TmuxCodexStatusHelper) -> String {
    format!(
        "#({})",
        tmux_codex_status_command_for_helper(workspace_root, helper)
    )
}

#[cfg(test)]
pub(super) fn tmux_codex_status_command(workspace_root: &Path) -> Result<String> {
    let helper = tmux_codex_status_helper_paths(workspace_root)?;
    Ok(tmux_codex_status_command_for_helper(
        workspace_root,
        &helper,
    ))
}

fn tmux_codex_status_command_for_helper(
    workspace_root: &Path,
    helper: &TmuxCodexStatusHelper,
) -> String {
    let script = shell_escape_tmux_status_word(&helper.script_path);
    let workspace = shell_escape_tmux_status_word(workspace_root);
    let cache = shell_escape_tmux_status_word(&helper.cache_path);
    format!("{script} {workspace} {cache} {TMUX_CODEX_LOC_CACHE_TTL_SECS}")
}

fn ensure_tmux_codex_status_helper(workspace_root: &Path) -> Result<TmuxCodexStatusHelper> {
    let candidates = tmux_codex_status_helper_path_candidates(workspace_root);
    let mut failures = Vec::new();
    for helper in candidates {
        match write_tmux_codex_status_helper(&helper) {
            Ok(()) => return Ok(helper),
            Err(err) => failures.push(format!("{}: {err:#}", helper.script_path.display())),
        }
    }
    bail!(
        "Could not write Codex tmux status helper: {}",
        failures.join("; ")
    )
}

fn write_tmux_codex_status_helper(helper: &TmuxCodexStatusHelper) -> Result<()> {
    let parent = helper
        .script_path
        .parent()
        .ok_or_else(|| anyhow!("invalid Codex tmux status helper path"))?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "create Codex tmux status helper directory {}",
            parent.display()
        )
    })?;
    write_file_atomically(&helper.script_path, TMUX_CODEX_STATUS_HELPER_SCRIPT).with_context(
        || {
            format!(
                "write Codex tmux status helper {}",
                helper.script_path.display()
            )
        },
    )?;
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(&helper.script_path)
            .with_context(|| {
                format!(
                    "read Codex tmux status helper metadata {}",
                    helper.script_path.display()
                )
            })?
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&helper.script_path, permissions).with_context(|| {
            format!(
                "set Codex tmux status helper permissions {}",
                helper.script_path.display()
            )
        })?;
    }
    Ok(())
}

#[cfg(test)]
fn tmux_codex_status_helper_paths(workspace_root: &Path) -> Result<TmuxCodexStatusHelper> {
    tmux_codex_status_helper_path_candidates(workspace_root)
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("could not resolve Codex tmux status helper path"))
}

fn tmux_codex_status_helper_path_candidates(workspace_root: &Path) -> Vec<TmuxCodexStatusHelper> {
    let hash = workspace_hash(workspace_root);
    let mut roots = Vec::new();
    if let Some(path) = env_path("XDG_RUNTIME_DIR") {
        roots.push(path.join(TMUX_CODEX_STATUS_DIR));
    }
    if let Ok(path) = state_home() {
        roots.push(path.join(TMUX_CODEX_STATUS_DIR));
    }
    roots.push(env::temp_dir().join(format!("{TMUX_CODEX_STATUS_DIR}-{}", process::id())));

    let mut helpers = Vec::new();
    for root in roots {
        let helper = TmuxCodexStatusHelper {
            script_path: root.join(format!("{hash}.sh")),
            cache_path: root.join(format!("{hash}.loc")),
        };
        if helpers
            .iter()
            .any(|existing: &TmuxCodexStatusHelper| existing.script_path == helper.script_path)
        {
            continue;
        }
        helpers.push(helper);
    }
    helpers
}

#[cfg(test)]
pub(super) fn tmux_codex_status_helper_script() -> &'static str {
    TMUX_CODEX_STATUS_HELPER_SCRIPT
}

fn shell_escape_tmux_status_word(path: &Path) -> String {
    let value = path.as_os_str().to_string_lossy();
    if value.is_empty() {
        return "''".to_string();
    }

    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || "_@%+=:,./-".contains(ch) {
            out.push(ch);
        } else {
            out.push('\\');
            out.push(ch);
        }
    }
    out
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
        bail!(
            "Could not list Codex sessions from tmux: {}",
            tmux_failure_detail(&stderr)
        );
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

fn tmux_failure_detail(stderr: &str) -> &str {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        "tmux returned a non-zero exit status without details"
    } else {
        trimmed
    }
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
