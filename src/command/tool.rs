//! Tool manager for versioned executables.

mod batch;
mod doctor;
mod integrations;
mod listing;
mod policy;
mod source;
mod state;

use anyhow::{Context, Result, anyhow, bail};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use regex::Regex;
use reqx::{
    advanced::{ClientProfile, RedirectPolicy},
    blocking::{Client, ClientBuilder},
    prelude::RetryPolicy,
};
use serde::{Deserialize, Serialize};
use signal_hook::{consts::signal::SIGINT, flag as signal_flag};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    env,
    fmt::Write as _,
    fs::{self, File, OpenOptions},
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use self::doctor::run_doctor;
use self::listing::{
    LatestCheck, LatestResolutionMode, UnmanagedBinary, list_installed, list_outdated,
    resolve_latest_checks_for_names_with_mode, show_catalog, show_tool,
};
#[cfg(test)]
use self::listing::{
    latest_check_progress_message, list_update_status, tool_update_cache_entry_is_fresh,
};
use self::policy::{
    GithubReleasePolicy, PackagePolicy, ToolLayout, ToolPolicy,
    canonical_tool_name as canonical_tool_name_impl, find_tool_policy, supported_tool_names_csv,
    tool_policies,
};
use self::source::{resolve_install_source, resolve_requested_version};
use self::{batch::*, integrations::*, state::*};
use crate::{
    cli::ToolCommands,
    command::{paths, render as text_render, style as tty_style, write_file_atomically, za_config},
};

const HTTP_TIMEOUT_SECS: u64 = 300;
const GITHUB_API_BASE: &str = "https://api.github.com";
const HTTP_USER_AGENT: &str = "za-tool-manager/0.1";
const MANIFEST_FILE: &str = "manifest.json";
const LOCK_FILE: &str = ".tool.lock";
const CURRENT_TMP_FILE_MARKER: &str = ".tmp-current-";
const SELF_UPDATE_BACKUP_DIR: &str = ".self-update";
const SELF_UPDATE_BACKUP_PREFIX: &str = "za-self-backup-";
const MANIFEST_SCHEMA_VERSION: u32 = 1;
const SOURCE_KIND_DOWNLOAD: &str = "download";
const SOURCE_KIND_ADOPTED: &str = "adopted";
const SOURCE_KIND_SYNTHESIZED: &str = "synthesized";
const IDE_TERMINAL_BASH_HELPER_START_MARKER: &str = "# >>> za ide-terminal (bash) >>>";
const IDE_TERMINAL_BASH_HELPER_END_MARKER: &str = "# <<< za ide-terminal (bash) <<<";
const STARSHIP_BASH_INIT_START_MARKER: &str = "# >>> za starship (bash) >>>";
const STARSHIP_BASH_INIT_END_MARKER: &str = "# <<< za starship (bash) <<<";
const BLESH_BASH_INIT_TOP_START_MARKER: &str = "# >>> za ble.sh (bash top) >>>";
const BLESH_BASH_INIT_TOP_END_MARKER: &str = "# <<< za ble.sh (bash top) <<<";
const BLESH_BASH_INIT_BOTTOM_START_MARKER: &str = "# >>> za ble.sh (bash bottom) >>>";
const BLESH_BASH_INIT_BOTTOM_END_MARKER: &str = "# <<< za ble.sh (bash bottom) <<<";
const PROXY_HINT: &str =
    "if your network requires a proxy, set HTTPS_PROXY/HTTP_PROXY (and optional NO_PROXY)";
const TOOL_UPDATE_CACHE_SCHEMA_VERSION: u32 = 3;
const TOOL_UPDATE_CACHE_FILE_NAME: &str = "tool-latest-cache-v3.json";
const TOOL_UPDATE_CACHE_TTL_SECS: u64 = 10 * 60;
const TOOL_UPDATE_JOBS_MULTIPLIER: usize = 2;
const TOOL_UPDATE_JOBS_MIN: usize = 2;
const TOOL_UPDATE_JOBS_MAX: usize = 8;
const TOOL_MATERIALIZE_JOBS_MAX: usize = 2;
const TOOL_EXIT_UPDATES_AVAILABLE: i32 = 20;
const TOOL_EXIT_UPDATE_CHECK_FAILED: i32 = 21;

static VERSION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bv?(\d+\.\d+\.\d+(?:[-+][0-9A-Za-z\.-]+)?)\b")
        .expect("version regex compiles")
});
static INTERRUPT_REQUESTED: LazyLock<Arc<AtomicBool>> =
    LazyLock::new(|| Arc::new(AtomicBool::new(false)));
static SIGNAL_HANDLER_REGISTRATION: LazyLock<Result<(), String>> = LazyLock::new(|| {
    signal_flag::register(SIGINT, Arc::clone(&INTERRUPT_REQUESTED))
        .map_err(|err| format!("register SIGINT handler: {err}"))?;
    #[cfg(unix)]
    signal_flag::register(
        signal_hook::consts::signal::SIGTERM,
        Arc::clone(&INTERRUPT_REQUESTED),
    )
    .map_err(|err| format!("register SIGTERM handler: {err}"))?;
    Ok(())
});

fn prepare_interruptible_tool_operation() -> Result<()> {
    if let Err(err) = &*SIGNAL_HANDLER_REGISTRATION {
        bail!("failed to initialize interrupt handlers: {err}");
    }
    INTERRUPT_REQUESTED.store(false, Ordering::SeqCst);
    let removed = source::cleanup_stale_temp_dirs();
    if removed > 0 {
        eprintln!("🧹 Cleaned {removed} stale temp dir(s) from previous interrupted runs");
    }
    Ok(())
}

fn print_tool_stage(stage: &str, message: impl AsRef<str>) {
    let stage_label = format!("{stage:<8}");
    let styled_stage = style_tool_stage_token(stage, &stage_label);
    if io::stdout().is_terminal() {
        let prefix = format!("{} {}", tool_stage_icon(stage), styled_stage);
        println!("{prefix} {}", style_tool_message(message.as_ref()));
    } else {
        println!("{styled_stage} {}", style_tool_message(message.as_ref()));
    }
}

fn print_tool_stage_if(enabled: bool, stage: &str, message: impl AsRef<str>) {
    if enabled {
        print_tool_stage(stage, message);
    }
}

fn tool_stage_icon(stage: &str) -> &'static str {
    match stage {
        "resolve" => "🔎",
        "update" => "⬆️",
        "sync" => "🔄",
        "repair" => "🔧",
        "source" => "📦",
        "install" => "📥",
        "activate" => "✅",
        "prune" => "🧹",
        "next" => "ℹ️",
        "done" => "✅",
        "fail" => "❌",
        _ => "•",
    }
}

fn style_tool_stage_token(stage: &str, value: &str) -> String {
    match stage {
        "done" | "activate" => tty_style::success(value),
        "update" | "install" | "sync" => tty_style::active(value),
        "repair" => tty_style::warning(value),
        "fail" => tty_style::error(value),
        _ => tty_style::dim(value),
    }
}

fn style_tool_message(message: &str) -> String {
    let message = style_backticked_segments(message)
        .replace("(dry-run)", &tty_style::dim("(dry-run)"))
        .replace("(no changes)", &tty_style::dim("(no changes)"))
        .replace(" -> ", &format!(" {} ", tty_style::dim("->")))
        .replace(
            " already at ",
            &format!(" {} ", tty_style::dim("already at")),
        )
        .replace(
            " no changes needed",
            &format!(" {}", tty_style::dim("no changes needed")),
        );
    if let Some((prefix, path)) = message.split_once(" from URL ") {
        format!(
            "{prefix} {} {}",
            tty_style::dim("from URL"),
            tty_style::dim(path)
        )
    } else {
        message
    }
}

fn style_backticked_segments(message: &str) -> String {
    let mut out = String::new();
    let mut rest = message;
    while let Some(start) = rest.find('`') {
        let (before, after_start) = rest.split_at(start);
        out.push_str(before);
        let after_start = &after_start[1..];
        if let Some(end) = after_start.find('`') {
            let (inner, after_end) = after_start.split_at(end);
            out.push_str(&tty_style::header(format!("`{inner}`")));
            rest = &after_end[1..];
        } else {
            out.push('`');
            out.push_str(after_start);
            return out;
        }
    }
    out.push_str(rest);
    out
}

fn new_tool_progress_bar(
    stage: &str,
    total: usize,
    message: impl Into<String>,
) -> Option<ProgressBar> {
    if total == 0 || !io::stderr().is_terminal() {
        return None;
    }

    let progress = ProgressBar::new(total as u64);
    progress.set_draw_target(ProgressDrawTarget::stderr_with_hz(10));
    progress.set_style(
        ProgressStyle::with_template(
            "{spinner} {prefix} [{bar:18.cyan/blue}] {pos}/{len} {wide_msg}",
        )
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("=> ")
        .tick_strings(&["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"]),
    );
    progress.enable_steady_tick(Duration::from_millis(80));
    progress.set_prefix(format!("{} {stage}", tool_stage_icon(stage)));
    progress.set_message(message.into());
    Some(progress)
}

fn install_source_failure_guidance(err: &anyhow::Error) -> Option<&'static str> {
    let message = format!("{err:#}").to_ascii_lowercase();
    if message.contains("timed out")
        || message.contains("connection")
        || message.contains("dns")
        || message.contains("proxy")
        || message.contains("status 5")
    {
        return Some("retryable failure: network or proxy access to the upstream release failed");
    }
    if message.contains("status 404")
        || message.contains("missing valid sha256 digest")
        || message.contains("sha256 mismatch")
        || message.contains("unsupported tool")
        || message.contains("does not contain expected asset")
    {
        return Some(
            "non-retryable failure: source policy or upstream release contents need attention",
        );
    }
    None
}

pub(super) fn is_interrupt_requested() -> bool {
    INTERRUPT_REQUESTED.load(Ordering::SeqCst)
}

pub(super) fn ensure_not_interrupted() -> Result<()> {
    if is_interrupt_requested() {
        bail!("operation interrupted by user");
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolScope {
    Global,
    User,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolScopeRequest {
    Auto,
    Global,
    User,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ToolUpdateChannel {
    Stable,
    CodexAlpha,
}

impl ToolScopeRequest {
    pub fn from_flags(user: bool, global: bool) -> Result<Self> {
        match (user, global) {
            (true, true) => bail!("`--user` and `--global` are mutually exclusive"),
            (true, false) => Ok(Self::User),
            (false, true) => Ok(Self::Global),
            (false, false) => Ok(Self::Auto),
        }
    }
}

impl ToolScope {
    fn resolve(request: ToolScopeRequest) -> Self {
        match request {
            ToolScopeRequest::Global => Self::Global,
            ToolScopeRequest::User => Self::User,
            ToolScopeRequest::Auto => default_tool_scope(),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::User => "user",
        }
    }
}

#[cfg(unix)]
fn default_tool_scope() -> ToolScope {
    if rustix::process::geteuid().as_raw() == 0 {
        ToolScope::Global
    } else {
        ToolScope::User
    }
}

#[cfg(not(unix))]
fn default_tool_scope() -> ToolScope {
    ToolScope::User
}

fn detect_current_za_scope() -> Option<ToolScope> {
    let executable = env::current_exe().ok()?;
    let user_home = ToolHome::for_scope(ToolScope::User).ok()?;
    let global_home = ToolHome::for_scope(ToolScope::Global).ok()?;
    classify_tool_executable_scope(&executable, "za", &user_home, &global_home)
}

fn classify_tool_executable_scope(
    executable: &Path,
    name: &str,
    user_home: &ToolHome,
    global_home: &ToolHome,
) -> Option<ToolScope> {
    if tool_home_owns_executable(user_home, name, executable) {
        return Some(ToolScope::User);
    }
    if tool_home_owns_executable(global_home, name, executable) {
        return Some(ToolScope::Global);
    }
    None
}

fn tool_home_owns_executable(home: &ToolHome, name: &str, executable: &Path) -> bool {
    let executable = normalize_existing_path(executable);
    let active_bin = normalize_existing_path(&home.bin_path(name));
    let store_root = normalize_existing_path(&home.name_dir(name));
    executable == active_bin || executable.starts_with(store_root)
}

fn normalize_existing_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn ensure_tool_home_ready(home: &ToolHome) -> Result<ToolLock> {
    if let Err(err) = home.ensure_layout() {
        if home.scope == ToolScope::Global {
            return Err(err).with_context(|| {
                "cannot initialize global tool directories. retry with `za tool --user ...` or run with elevated privileges"
                    .to_string()
            });
        }
        return Err(err);
    }
    match ToolLock::acquire(home) {
        Ok(lock) => Ok(lock),
        Err(err) if home.scope == ToolScope::Global => Err(err).with_context(|| {
            "cannot acquire global tool lock. retry with `za tool --user ...` or run with elevated privileges"
                .to_string()
        }),
        Err(err) => Err(err),
    }
}

fn run_mutating_tool_command<F>(home: &ToolHome, action: F) -> Result<i32>
where
    F: FnOnce() -> Result<()>,
{
    let _lock = ensure_tool_home_ready(home)?;
    action()?;
    Ok(0)
}

fn run_ls_command(
    home: &ToolHome,
    tools: &[String],
    json: bool,
    supported: bool,
    outdated: bool,
    fail_on_updates: bool,
    fail_on_check_errors: bool,
) -> Result<i32> {
    if supported && outdated {
        bail!("`--supported` cannot be combined with `--outdated`");
    }
    if supported && !tools.is_empty() {
        bail!("`za tool ls --supported` does not accept tool names");
    }
    if supported && (fail_on_updates || fail_on_check_errors) {
        bail!("`--fail-on-updates`/`--fail-on-check-errors` require `--outdated`");
    }
    if !outdated && (fail_on_updates || fail_on_check_errors) {
        bail!("`--fail-on-updates`/`--fail-on-check-errors` require `--outdated`");
    }

    if supported {
        return show_catalog(json);
    }
    if outdated {
        return list_outdated(home, tools, json, fail_on_updates, fail_on_check_errors);
    }
    if !tools.is_empty() {
        bail!("`za tool ls` does not accept tool names; use `za tool show <tool>`");
    }
    list_installed(home, json)
}

pub fn run(cmd: ToolCommands, scope_request: ToolScopeRequest) -> Result<i32> {
    prepare_interruptible_tool_operation()?;

    let home = ToolHome::detect(scope_request)?;
    cleanup_legacy_current_dir_artifacts(&home)?;

    match cmd {
        ToolCommands::Ls {
            tools,
            json,
            supported,
            outdated,
            fail_on_updates,
            fail_on_check_errors,
        } => run_ls_command(
            &home,
            &tools,
            json,
            supported,
            outdated,
            fail_on_updates,
            fail_on_check_errors,
        ),
        ToolCommands::Show { tool, json, path } => {
            if path {
                print_active_managed_path(&home, &tool)?;
                Ok(0)
            } else {
                show_tool(&home, &tool, json)
            }
        }
        ToolCommands::Install {
            tools,
            version,
            adopt,
            dry_run,
            verbose,
        } => {
            if dry_run {
                install_tools(
                    &home,
                    &tools,
                    version.as_deref(),
                    adopt,
                    ToolAction::Install,
                    true,
                    verbose,
                )?;
                Ok(0)
            } else {
                let home_for_action = home.clone();
                run_mutating_tool_command(&home, move || {
                    install_tools(
                        &home_for_action,
                        &tools,
                        version.as_deref(),
                        adopt,
                        ToolAction::Install,
                        false,
                        verbose,
                    )
                })
            }
        }
        ToolCommands::Update {
            all,
            tools,
            version,
            alpha,
            dry_run,
            verbose,
        } => {
            if dry_run {
                update_tools(&home, all, &tools, version.as_deref(), alpha, true, verbose)?;
                Ok(0)
            } else {
                let home_for_action = home.clone();
                run_mutating_tool_command(&home, move || {
                    update_tools(
                        &home_for_action,
                        all,
                        &tools,
                        version.as_deref(),
                        alpha,
                        false,
                        verbose,
                    )
                })
            }
        }
        ToolCommands::Sync {
            file,
            dry_run,
            verbose,
        } => {
            if dry_run {
                sync_manifest(&home, &file, true, verbose)?;
                Ok(0)
            } else {
                let home_for_action = home.clone();
                run_mutating_tool_command(&home, move || {
                    sync_manifest(&home_for_action, &file, false, verbose)
                })
            }
        }
        ToolCommands::Doctor { tools, json } => run_doctor(&home, &tools, json),
        ToolCommands::Uninstall { tool, version } => {
            let home_for_action = home.clone();
            run_mutating_tool_command(&home, move || {
                uninstall(
                    &home_for_action,
                    ToolSpec::from_args(&tool, version.as_deref())?,
                )
            })
        }
        ToolCommands::Which { tool } => {
            print_active_managed_path(&home, &tool)?;
            Ok(0)
        }
        ToolCommands::Catalog { json } => show_catalog(json),
        ToolCommands::Outdated {
            tools,
            json,
            fail_on_updates,
            fail_on_check_errors,
        } => list_outdated(&home, &tools, json, fail_on_updates, fail_on_check_errors),
        ToolCommands::Adopt { tool } => {
            let home_for_action = home.clone();
            run_mutating_tool_command(&home, move || {
                install_tools(
                    &home_for_action,
                    std::slice::from_ref(&tool),
                    None,
                    true,
                    ToolAction::Install,
                    false,
                    false,
                )
            })
        }
    }
}

pub fn update_self(
    scope_request: ToolScopeRequest,
    check: bool,
    version: Option<String>,
) -> Result<i32> {
    prepare_interruptible_tool_operation()?;

    let home = ToolHome::detect_for_self_update(scope_request)?;
    cleanup_legacy_current_dir_artifacts(&home)?;

    if check {
        return check_self_update(&version);
    }

    if let Err(err) = home.ensure_layout() {
        if home.scope == ToolScope::Global {
            return Err(err).with_context(|| {
                "cannot initialize global update directories. retry with `sudo /usr/local/bin/za update --global`"
                    .to_string()
            });
        }
        return Err(err);
    }
    let _lock = match ToolLock::acquire(&home) {
        Ok(lock) => lock,
        Err(err) if home.scope == ToolScope::Global => {
            return Err(err).with_context(|| {
                "cannot acquire global update lock. retry with `sudo /usr/local/bin/za update --global`"
                    .to_string()
            });
        }
        Err(err) => return Err(err),
    };

    let requested = version.as_deref();
    let target_version = resolve_requested_version("za", requested, za_config::ProxyScope::Update)?;
    let target_spec = format!("za:{target_version}");
    let previous_active = read_current_version(&home, "za")?;
    let backup = backup_existing_self_binary(&home)?;

    let installed = install(
        &home,
        ToolSpec::parse(&target_spec)?,
        InstallOptions::update(za_config::ProxyScope::Update).with_prune(false),
    )?;
    if let Err(err) = verify_self_update(&home, &installed.tool) {
        let rollback_res =
            rollback_self_update(&home, previous_active.as_deref(), backup.as_deref());
        if let Some(path) = backup.as_ref() {
            let _ = fs::remove_file(path);
        }
        return match rollback_res {
            Ok(()) => Err(err.context("self-update health check failed; rollback applied")),
            Err(rollback_err) => Err(err.context(format!(
                "self-update health check failed; rollback also failed: {rollback_err:#}"
            ))),
        };
    }

    if let Some(path) = backup.as_ref() {
        let _ = fs::remove_file(path);
    }
    let removed = prune_non_active_versions(&home, &installed.tool)?;
    if !removed.is_empty() {
        print_tool_stage(
            "prune",
            format!("removed old `za` versions: {}", removed.join(", ")),
        );
    }
    print_tool_stage(
        "done",
        format!("self-update complete: {}", installed.tool.image()),
    );
    Ok(0)
}

fn check_self_update(requested_version: &Option<String>) -> Result<i32> {
    let current = normalize_version(env!("CARGO_PKG_VERSION"));
    let target = resolve_requested_version(
        "za",
        requested_version.as_deref(),
        za_config::ProxyScope::Update,
    )?;

    println!("Current za: {current}");
    if requested_version.is_some() {
        println!("Requested za: {target}");
    } else {
        println!("Latest za: {target}");
    }

    if current == target {
        println!("✅ za is up-to-date");
    } else {
        println!("⬆️  Update available: {current} -> {target}");
    }
    Ok(0)
}

fn backup_existing_self_binary(home: &ToolHome) -> Result<Option<PathBuf>> {
    let bin = home.bin_path("za");
    if !bin.exists() {
        return Ok(None);
    }

    let backup_dir = home.self_update_backup_dir();
    fs::create_dir_all(&backup_dir)?;
    let backup = backup_dir.join(format!("{SELF_UPDATE_BACKUP_PREFIX}{}", std::process::id()));
    fs::copy(&bin, &backup).with_context(|| {
        format!(
            "backup current za binary {} -> {}",
            bin.display(),
            backup.display()
        )
    })?;

    #[cfg(unix)]
    {
        let mode = fs::metadata(&bin)?.permissions().mode();
        fs::set_permissions(&backup, fs::Permissions::from_mode(mode))?;
    }

    Ok(Some(backup))
}

fn verify_self_update(home: &ToolHome, installed: &ToolRef) -> Result<()> {
    let bin = home.bin_path("za");
    let output = Command::new(&bin)
        .arg("--version")
        .output()
        .with_context(|| format!("run self-update health check {}", bin.display()))?;
    if !output.status.success() {
        bail!(
            "self-update health check failed: `{}` exited with status {}",
            bin.display(),
            output.status
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let merged = format!("{stdout}\n{stderr}");
    let Some(actual) = extract_version_from_text(&merged) else {
        bail!("self-update health check failed: cannot parse version from `--version` output");
    };
    if normalize_version(&actual) != normalize_version(&installed.version) {
        bail!(
            "self-update health check failed: expected {}, got {}",
            installed.version,
            actual
        );
    }

    Ok(())
}

fn rollback_self_update(
    home: &ToolHome,
    previous_active_version: Option<&str>,
    backup_path: Option<&Path>,
) -> Result<()> {
    if let Some(previous) = previous_active_version {
        let previous_tool = ToolRef {
            name: "za".to_string(),
            version: previous.to_string(),
        };
        if home.install_path(&previous_tool).exists() {
            activate_tool(home, &previous_tool)?;
            println!(
                "↩️  Rolled back to managed version {}",
                previous_tool.image()
            );
            return Ok(());
        }
    }

    if let Some(backup) = backup_path {
        copy_executable(backup, &home.bin_path("za"))?;
        remove_file_if_exists(&home.current_file("za"))?;
        println!("↩️  Rolled back to previous unmanaged za binary");
        return Ok(());
    }

    bail!("no rollback target available")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolAction {
    Install,
    Update,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagedFileChange {
    Created,
    Updated,
    Unchanged,
}

impl ManagedFileChange {
    fn label(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Updated => "updated",
            Self::Unchanged => "already present",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagedBlockPosition {
    Top,
    Bottom,
    AfterMarker(&'static str),
    BeforeMarker(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdoptionMode {
    Disallow,
    Require,
}

#[derive(Debug, Clone, Copy)]
struct InstallOptions {
    action: ToolAction,
    adoption: AdoptionMode,
    prune_after_activation: bool,
    proxy_scope: za_config::ProxyScope,
    dry_run: bool,
    emit_stages: bool,
    emit_plan_stage: bool,
    download_display: source::DownloadDisplay,
}

impl InstallOptions {
    fn install(proxy_scope: za_config::ProxyScope) -> Self {
        Self {
            action: ToolAction::Install,
            adoption: AdoptionMode::Disallow,
            prune_after_activation: true,
            proxy_scope,
            dry_run: false,
            emit_stages: true,
            emit_plan_stage: false,
            download_display: source::DownloadDisplay::Detailed,
        }
    }

    fn update(proxy_scope: za_config::ProxyScope) -> Self {
        Self {
            action: ToolAction::Update,
            adoption: AdoptionMode::Disallow,
            prune_after_activation: true,
            proxy_scope,
            dry_run: false,
            emit_stages: true,
            emit_plan_stage: false,
            download_display: source::DownloadDisplay::Detailed,
        }
    }

    fn adopt(proxy_scope: za_config::ProxyScope) -> Self {
        Self {
            action: ToolAction::Install,
            adoption: AdoptionMode::Require,
            prune_after_activation: false,
            proxy_scope,
            dry_run: false,
            emit_stages: true,
            emit_plan_stage: false,
            download_display: source::DownloadDisplay::Detailed,
        }
    }

    fn with_prune(mut self, prune_after_activation: bool) -> Self {
        self.prune_after_activation = prune_after_activation;
        self
    }

    fn dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }

    fn emit_stages(mut self, emit_stages: bool) -> Self {
        self.emit_stages = emit_stages;
        self
    }

    fn emit_plan_stage(mut self, emit_plan_stage: bool) -> Self {
        self.emit_plan_stage = emit_plan_stage;
        self
    }

    fn download_display(mut self, download_display: source::DownloadDisplay) -> Self {
        self.download_display = download_display;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallOutcome {
    Installed,
    Updated,
    Repaired,
    Unchanged,
}

#[derive(Debug, Clone)]
struct InstallResult {
    tool: ToolRef,
    outcome: InstallOutcome,
}

#[derive(Debug, Clone)]
struct InstallPlan {
    tool: ToolRef,
    adoption: Option<AdoptionCandidate>,
    previous_active: Option<String>,
    already_installed: bool,
    planned_outcome: InstallOutcome,
    current_matches_target: bool,
}

#[derive(Debug)]
enum PullArtifactKind {
    File,
    Archive,
}

#[derive(Debug)]
struct PullSource {
    kind: &'static str,
    artifact: PullArtifactKind,
    path: PathBuf,
    resolved_by: String,
    cleanup_root: Option<PathBuf>,
}

impl PullSource {
    fn temp(
        kind: &'static str,
        artifact: PullArtifactKind,
        path: PathBuf,
        resolved_by: String,
        cleanup_root: PathBuf,
    ) -> Self {
        Self {
            kind,
            artifact,
            path,
            resolved_by,
            cleanup_root: Some(cleanup_root),
        }
    }
}

impl Drop for PullSource {
    fn drop(&mut self) {
        if let Some(root) = &self.cleanup_root {
            source::unregister_temp_dir(root);
            let _ = fs::remove_dir_all(root);
        }
    }
}

#[derive(Debug, Clone)]
struct InstallSource {
    kind: &'static str,
    detail: String,
}

#[derive(Debug, Clone)]
struct AdoptionCandidate {
    path: PathBuf,
    version: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ToolManifest {
    schema_version: u32,
    name: String,
    version: String,
    installed_at_unix_secs: u64,
    source_kind: String,
    source_detail: String,
    sha256: String,
    size_bytes: u64,
}

#[derive(Debug, Deserialize)]
struct ToolSyncManifest {
    tools: Vec<String>,
}

#[derive(Debug)]
struct ToolLock {
    _file: File,
}

#[derive(Debug, Clone)]
struct ToolRef {
    name: String,
    version: String,
}

#[derive(Debug, Clone)]
struct ToolSpec {
    name: String,
    version: Option<String>,
}

impl ToolSpec {
    fn from_args(name: &str, version: Option<&str>) -> Result<Self> {
        let trimmed_name = name.trim();
        if trimmed_name.is_empty() {
            bail!("tool name must not be empty");
        }
        validate_name(trimmed_name)?;
        let version = version
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(normalize_version);
        Ok(Self {
            name: trimmed_name.to_string(),
            version,
        })
    }

    fn parse(input: &str) -> Result<Self> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            bail!("tool spec must not be empty");
        }
        if trimmed.contains('@') && trimmed.contains(':') {
            bail!("invalid tool spec `{input}`: use either `name:version` or `name@version`");
        }
        let (name, version) = trimmed
            .split_once('@')
            .or_else(|| trimmed.split_once(':'))
            .map_or((trimmed, None), |(name, version)| (name, Some(version)));
        if version.is_some_and(|version| version.trim().is_empty()) {
            bail!("invalid tool spec `{input}`: version must not be empty");
        }
        Self::from_args(name, version)
    }

    fn resolve(self, resolved_version: String) -> ToolRef {
        ToolRef {
            name: self.name,
            version: resolved_version,
        }
    }
}

impl ToolRef {
    #[cfg(test)]
    fn parse(input: &str) -> Result<Self> {
        if input.contains('@') && input.contains(':') {
            bail!("invalid tool ref `{input}`: use either `name:version` or `name@version`");
        }
        let (name, version) = input
            .split_once('@')
            .or_else(|| input.split_once(':'))
            .ok_or_else(|| {
                anyhow!("invalid tool ref `{input}`: expected `name:version` or `name@version`")
            })?;
        let spec = ToolSpec::from_args(name, Some(version))?;
        let Some(version) = spec.version else {
            bail!("invalid tool ref `{input}`: version must not be empty");
        };
        Ok(Self {
            name: spec.name,
            version,
        })
    }

    fn image(&self) -> String {
        format!("{}:{}", self.name, self.version)
    }
}

fn validate_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        bail!("tool name must not be empty");
    }
    if name.contains('/') || name.contains('\\') {
        bail!("tool name `{name}` must not contain path separators");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        bail!("tool name `{name}` contains unsupported characters");
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct ToolHome {
    scope: ToolScope,
    store_dir: PathBuf,
    current_dir: PathBuf,
    bin_dir: PathBuf,
}

impl ToolHome {
    fn detect(scope_request: ToolScopeRequest) -> Result<Self> {
        let scope = ToolScope::resolve(scope_request);
        Self::for_scope(scope)
    }

    fn detect_for_self_update(scope_request: ToolScopeRequest) -> Result<Self> {
        let scope = match scope_request {
            ToolScopeRequest::Auto => detect_current_za_scope().unwrap_or_else(default_tool_scope),
            ToolScopeRequest::Global => ToolScope::Global,
            ToolScopeRequest::User => ToolScope::User,
        };
        Self::for_scope(scope)
    }

    fn for_scope(scope: ToolScope) -> Result<Self> {
        match scope {
            ToolScope::Global => Ok(Self {
                scope,
                store_dir: PathBuf::from(paths::GLOBAL_TOOL_STORE_DIR),
                current_dir: PathBuf::from(paths::GLOBAL_TOOL_CURRENT_DIR),
                bin_dir: PathBuf::from(paths::GLOBAL_BIN_DIR),
            }),
            ToolScope::User => Ok(Self {
                scope,
                store_dir: paths::user_tool_store_dir()?,
                current_dir: paths::user_tool_current_dir()?,
                bin_dir: paths::user_bin_dir()?,
            }),
        }
    }

    fn ensure_layout(&self) -> Result<()> {
        create_dir_all_with_context(&self.store_dir, "store")?;
        create_dir_all_with_context(&self.current_dir, "current")?;
        create_dir_all_with_context(&self.bin_dir, "bin")?;
        Ok(())
    }

    fn install_path(&self, tool: &ToolRef) -> PathBuf {
        match package_policy_for_name(&tool.name) {
            Some(package) => self.package_payload_dir(tool).join(package.entry_relpath),
            None => self
                .store_dir
                .join(&tool.name)
                .join(&tool.version)
                .join(&tool.name),
        }
    }

    fn version_dir(&self, tool: &ToolRef) -> PathBuf {
        self.store_dir.join(&tool.name).join(&tool.version)
    }

    fn package_payload_dir(&self, tool: &ToolRef) -> PathBuf {
        self.version_dir(tool).join("payload")
    }

    fn manifest_path(&self, tool: &ToolRef) -> PathBuf {
        self.version_dir(tool).join(MANIFEST_FILE)
    }

    fn name_dir(&self, name: &str) -> PathBuf {
        self.store_dir.join(name)
    }

    fn current_file(&self, name: &str) -> PathBuf {
        self.current_dir.join(name)
    }

    fn current_package_path(&self, name: &str) -> PathBuf {
        self.current_dir.join(format!("{name}.payload"))
    }

    fn bin_path(&self, name: &str) -> PathBuf {
        self.bin_dir.join(name)
    }

    fn active_path(&self, name: &str) -> PathBuf {
        match package_policy_for_name(name) {
            Some(package) => self.current_package_path(name).join(package.entry_relpath),
            None => self.bin_path(name),
        }
    }

    fn lock_file(&self) -> PathBuf {
        self.current_dir.join(LOCK_FILE)
    }

    fn self_update_backup_dir(&self) -> PathBuf {
        self.current_dir.join(SELF_UPDATE_BACKUP_DIR)
    }
}

impl ToolLock {
    fn acquire(home: &ToolHome) -> Result<Self> {
        let lock_path = home.lock_file();
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("open lock file {}", lock_path.display()))?;
        file.lock()
            .with_context(|| format!("acquire lock {}", lock_path.display()))?;
        Ok(Self { _file: file })
    }
}

impl Drop for ToolLock {
    fn drop(&mut self) {
        let _ = self._file.unlock();
    }
}

fn materialize_pulled_tool(home: &ToolHome, tool: &ToolRef, source: &PullSource) -> Result<()> {
    match tool_layout_for_name(&tool.name) {
        ToolLayout::Binary => {
            if !matches!(source.artifact, PullArtifactKind::File) {
                bail!(
                    "resolved install source for `{}` was not a direct executable payload",
                    tool.name
                );
            }
            copy_executable(&source.path, &home.install_path(tool))
        }
        ToolLayout::Package => stage_package_payload(home, tool, source),
    }
}

fn stage_package_payload(home: &ToolHome, tool: &ToolRef, source: &PullSource) -> Result<()> {
    if !matches!(source.artifact, PullArtifactKind::Archive) {
        bail!(
            "resolved install source for `{}` was not an archive payload",
            tool.name
        );
    }

    let version_dir = home.version_dir(tool);
    let unpack_dir = version_dir.join(".unpack");
    let payload_dir = home.package_payload_dir(tool);
    let run = (|| -> Result<()> {
        remove_path_if_exists(&unpack_dir)?;
        fs::create_dir_all(&version_dir)?;
        source::extract_archive_into_dir(&source.path, &unpack_dir)?;
        let staged_root = select_extracted_payload_root(&unpack_dir)?;
        remove_path_if_exists(&payload_dir)?;
        if staged_root == unpack_dir {
            fs::rename(&unpack_dir, &payload_dir).with_context(|| {
                format!(
                    "stage package payload {} -> {}",
                    unpack_dir.display(),
                    payload_dir.display()
                )
            })?;
        } else {
            fs::rename(&staged_root, &payload_dir).with_context(|| {
                format!(
                    "stage package payload {} -> {}",
                    staged_root.display(),
                    payload_dir.display()
                )
            })?;
            remove_path_if_exists(&unpack_dir)?;
        }
        let entry_path = home.install_path(tool);
        if !entry_path.exists() {
            bail!(
                "package payload for `{}` missing expected entry {}",
                tool.name,
                entry_path.display()
            );
        }
        Ok(())
    })();

    if run.is_err() {
        let _ = remove_path_if_exists(&version_dir);
    }
    run
}

fn select_extracted_payload_root(unpack_dir: &Path) -> Result<PathBuf> {
    let mut dirs = Vec::new();
    let mut other_entries = Vec::new();
    for entry in
        fs::read_dir(unpack_dir).with_context(|| format!("read {}", unpack_dir.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            dirs.push(entry.path());
        } else {
            other_entries.push(entry.path());
        }
    }

    if dirs.len() == 1 && other_entries.is_empty() {
        Ok(dirs.remove(0))
    } else {
        Ok(unpack_dir.to_path_buf())
    }
}

fn install(home: &ToolHome, requested: ToolSpec, options: InstallOptions) -> Result<InstallResult> {
    let plan = plan_install(home, requested, options)?;

    emit_install_plan_stage(
        &plan.tool,
        plan.previous_active.as_deref(),
        plan.planned_outcome,
        plan.current_matches_target,
        options,
    );

    if update_plan_is_unchanged(&plan, options) {
        if options.dry_run && options.emit_stages {
            print_tool_stage("next", "no changes needed");
        }
        return Ok(InstallResult {
            tool: plan.tool,
            outcome: InstallOutcome::Unchanged,
        });
    }

    if options.dry_run {
        preview_install(home, &plan, options)?;
        return Ok(InstallResult {
            tool: plan.tool,
            outcome: plan.planned_outcome,
        });
    }

    materialize_install_plan(home, &plan, options, None)?;
    activate_install_plan(home, &plan, options)?;

    Ok(InstallResult {
        tool: plan.tool,
        outcome: plan.planned_outcome,
    })
}

fn plan_install(
    home: &ToolHome,
    mut requested: ToolSpec,
    options: InstallOptions,
) -> Result<InstallPlan> {
    ensure_not_interrupted()?;

    requested.name = canonical_tool_name(&requested.name);
    let adoption = if options.adoption == AdoptionMode::Require {
        detect_adoption_candidate(home, &requested)?
    } else {
        None
    };
    let version = if let Some(v) = requested.version.as_deref() {
        let v = normalize_version(v);
        if v.is_empty() {
            bail!("version must not be empty");
        }
        v
    } else if let Some(adopted) = adoption.as_ref() {
        adopted.version.clone()
    } else {
        print_tool_stage_if(
            options.emit_stages,
            "resolve",
            format!("latest version for `{}`", requested.name),
        );
        resolve_requested_version(&requested.name, None, options.proxy_scope)?
    };
    if options.adoption == AdoptionMode::Require && adoption.is_none() {
        bail!(
            "no unmanaged `{}` binary found in {} scope to adopt",
            requested.name,
            home.scope.label()
        );
    }
    let tool = requested.resolve(version);
    ensure_not_interrupted()?;

    let previous_active = read_current_version(home, &tool.name)?;
    let dst = home.install_path(&tool);
    let already_installed = dst.exists();
    let manifest_exists = home.manifest_path(&tool).exists();
    let active_exists = home.active_path(&tool.name).exists();
    let current_matches_target = previous_active
        .as_deref()
        .is_some_and(|current| normalize_version(current) == normalize_version(&tool.version));
    let update_target_is_healthy =
        current_matches_target && already_installed && manifest_exists && active_exists;
    let planned_outcome = match options.action {
        ToolAction::Install => {
            if update_target_is_healthy {
                InstallOutcome::Unchanged
            } else if already_installed && current_matches_target {
                InstallOutcome::Repaired
            } else {
                InstallOutcome::Installed
            }
        }
        ToolAction::Update => {
            if update_target_is_healthy {
                InstallOutcome::Unchanged
            } else if previous_active.as_deref().is_some_and(|current| {
                normalize_version(current) != normalize_version(&tool.version)
            }) {
                InstallOutcome::Updated
            } else {
                InstallOutcome::Repaired
            }
        }
    };

    Ok(InstallPlan {
        tool,
        adoption,
        previous_active,
        already_installed,
        planned_outcome,
        current_matches_target,
    })
}

fn update_plan_is_unchanged(plan: &InstallPlan, options: InstallOptions) -> bool {
    options.action == ToolAction::Update && plan.planned_outcome == InstallOutcome::Unchanged
}

fn materialize_install_plan(
    home: &ToolHome,
    plan: &InstallPlan,
    options: InstallOptions,
    download_progress_sink: Option<source::DownloadProgressSink>,
) -> Result<()> {
    let tool = &plan.tool;
    if !plan.already_installed {
        let dst = home.install_path(tool);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }

        let source =
            if let Some(adopted) = plan.adoption.as_ref().filter(|a| a.version == tool.version) {
                print_tool_stage_if(
                    options.emit_stages,
                    "source",
                    format!("adopting existing binary {}", adopted.path.display()),
                );
                copy_executable(&adopted.path, &dst)?;
                InstallSource {
                    kind: SOURCE_KIND_ADOPTED,
                    detail: format!("existing binary {}", adopted.path.display()),
                }
            } else {
                ensure_not_interrupted()?;
                print_tool_stage_if(
                    options.emit_stages,
                    "source",
                    format!("fetching `{}` {}", tool.name, tool.version),
                );
                let src = match resolve_install_source(
                    tool,
                    options.proxy_scope,
                    options.download_display,
                    download_progress_sink,
                ) {
                    Ok(src) => src,
                    Err(err) => {
                        return Err(match install_source_failure_guidance(&err) {
                            Some(guidance) => err.context(guidance),
                            None => err,
                        });
                    }
                };
                ensure_not_interrupted()?;
                materialize_pulled_tool(home, tool, &src)?;
                InstallSource {
                    kind: src.kind,
                    detail: src.resolved_by.clone(),
                }
            };
        write_manifest(home, tool, &source)?;
        print_tool_stage_if(
            options.emit_stages,
            "install",
            format!("{} from {}", tool.image(), source.detail),
        );
    } else {
        ensure_manifest(home, tool)?;
        print_tool_stage_if(
            options.emit_stages,
            "install",
            format!("already installed {}", tool.image()),
        );
    }
    Ok(())
}

fn activate_install_plan(
    home: &ToolHome,
    plan: &InstallPlan,
    options: InstallOptions,
) -> Result<()> {
    let tool = &plan.tool;
    activate_tool(home, tool)?;
    print_tool_stage_if(
        options.emit_stages,
        "activate",
        format!(
            "{} (path: {})",
            tool.image(),
            home.active_path(&tool.name).display()
        ),
    );
    emit_user_path_hint(home, tool, options);
    ensure_post_activation_integrations(home, tool, options.emit_stages)?;
    if options.prune_after_activation {
        let removed = prune_non_active_versions(home, tool)?;
        if !removed.is_empty() {
            print_tool_stage_if(
                options.emit_stages,
                "prune",
                format!(
                    "removed old `{}` versions: {}",
                    tool.name,
                    removed.join(", ")
                ),
            );
        }
    }
    Ok(())
}

fn emit_user_path_hint(home: &ToolHome, tool: &ToolRef, options: InstallOptions) {
    if home.scope != ToolScope::User || tool_layout_for_name(&tool.name) != ToolLayout::Binary {
        return;
    }
    if path_env_contains_dir(&home.bin_dir) {
        return;
    }
    let enabled = options.emit_stages || options.emit_plan_stage;
    print_tool_stage_if(
        enabled,
        "next",
        format!(
            "{} is not in PATH; add it for direct `{}` usage, or run through `za run {}`",
            home.bin_dir.display(),
            tool.name,
            tool.name
        ),
    );
}

fn path_env_contains_dir(dir: &Path) -> bool {
    let Some(path) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&path).any(|entry| entry == dir)
}

fn emit_install_plan_stage(
    tool: &ToolRef,
    previous_active: Option<&str>,
    planned_outcome: InstallOutcome,
    current_matches_target: bool,
    options: InstallOptions,
) {
    if options.emit_stages {
        emit_detailed_install_plan_stage(
            tool,
            previous_active,
            planned_outcome,
            current_matches_target,
            options,
        );
        return;
    }

    if !options.emit_plan_stage {
        return;
    }

    let Some((stage, mut message)) = compact_install_plan(tool, previous_active, planned_outcome)
    else {
        return;
    };
    if options.dry_run {
        message.push_str(" (dry-run)");
    }
    print_tool_stage(stage, message);
}

fn emit_detailed_install_plan_stage(
    tool: &ToolRef,
    previous_active: Option<&str>,
    planned_outcome: InstallOutcome,
    current_matches_target: bool,
    options: InstallOptions,
) {
    if options.action != ToolAction::Update {
        return;
    }

    match planned_outcome {
        InstallOutcome::Unchanged => {
            print_tool_stage(
                "update",
                format!("`{}` already at {} (no changes)", tool.name, tool.version),
            );
        }
        InstallOutcome::Repaired if current_matches_target => {
            print_tool_stage("repair", format!("`{}` {}", tool.name, tool.version));
        }
        InstallOutcome::Repaired => {
            print_tool_stage("repair", format!("`{}` -> {}", tool.name, tool.version));
        }
        InstallOutcome::Updated => match previous_active {
            Some(current) => print_tool_stage(
                "update",
                format!("`{}` {} -> {}", tool.name, current, tool.version),
            ),
            None => print_tool_stage("update", format!("`{}` -> {}", tool.name, tool.version)),
        },
        InstallOutcome::Installed => {}
    }
}

fn compact_install_plan(
    tool: &ToolRef,
    previous_active: Option<&str>,
    planned_outcome: InstallOutcome,
) -> Option<(&'static str, String)> {
    match planned_outcome {
        InstallOutcome::Installed => Some(("install", format!("`{}` {}", tool.name, tool.version))),
        InstallOutcome::Updated => Some((
            "update",
            match previous_active {
                Some(current) => format!("`{}` {} -> {}", tool.name, current, tool.version),
                None => format!("`{}` -> {}", tool.name, tool.version),
            },
        )),
        InstallOutcome::Repaired => Some((
            "repair",
            match previous_active {
                Some(current) if normalize_version(current) != normalize_version(&tool.version) => {
                    format!("`{}` {} -> {}", tool.name, current, tool.version)
                }
                _ => format!("`{}` {}", tool.name, tool.version),
            },
        )),
        InstallOutcome::Unchanged => None,
    }
}

fn preview_install(home: &ToolHome, plan: &InstallPlan, options: InstallOptions) -> Result<()> {
    let tool = &plan.tool;
    if !plan.already_installed {
        if let Some(adopted) = plan.adoption.as_ref().filter(|a| a.version == tool.version) {
            print_tool_stage_if(
                options.emit_stages,
                "source",
                format!("would adopt existing binary {}", adopted.path.display()),
            );
        } else {
            let source = match source::preview_install_source(tool, options.proxy_scope) {
                Ok(source) => source,
                Err(err) => {
                    return Err(match install_source_failure_guidance(&err) {
                        Some(guidance) => err.context(guidance),
                        None => err,
                    });
                }
            };
            print_tool_stage_if(
                options.emit_stages,
                "source",
                format!(
                    "would fetch `{}` {} from {}",
                    tool.name, tool.version, source.detail
                ),
            );
        }
        print_tool_stage_if(
            options.emit_stages,
            "install",
            format!("would install {}", tool.image()),
        );
    } else {
        print_tool_stage_if(
            options.emit_stages,
            "install",
            format!("already installed {}", tool.image()),
        );
    }

    print_tool_stage_if(
        options.emit_stages,
        "activate",
        format!(
            "would activate {} (path: {})",
            tool.image(),
            home.active_path(&tool.name).display()
        ),
    );
    preview_post_activation_integrations(home, tool, options.emit_stages)?;
    if options.prune_after_activation {
        let removed = stale_versions_to_prune(home, tool)?;
        if !removed.is_empty() {
            print_tool_stage_if(
                options.emit_stages,
                "prune",
                format!(
                    "would remove old `{}` versions: {}",
                    tool.name,
                    removed.join(", ")
                ),
            );
        }
    }
    print_tool_stage_if(
        options.emit_stages,
        "next",
        "dry-run only; no changes were made",
    );
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolBatchKind {
    Install,
    Update,
    Sync,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ToolBatchSummary {
    installed: usize,
    updated: usize,
    repaired: usize,
    unchanged: usize,
    skipped: usize,
    failed: usize,
}

impl ToolBatchSummary {
    fn record(self, outcome: InstallOutcome) -> Self {
        let mut updated = self;
        match outcome {
            InstallOutcome::Installed => updated.installed += 1,
            InstallOutcome::Updated => updated.updated += 1,
            InstallOutcome::Repaired => updated.repaired += 1,
            InstallOutcome::Unchanged => updated.unchanged += 1,
        }
        updated
    }
}

fn normalize_version(version: &str) -> String {
    version.trim_start_matches('v').to_string()
}

pub(crate) fn canonical_tool_name(name: &str) -> String {
    canonical_tool_name_impl(name)
}

pub(crate) fn managed_executable_path(name: &str, version: &str, store_dir: &Path) -> PathBuf {
    match package_policy_for_name(name) {
        Some(package) => store_dir
            .join(name)
            .join(version)
            .join("payload")
            .join(package.entry_relpath),
        None => store_dir.join(name).join(version).join(name),
    }
}

fn canonical_supported_tool_name(name: &str) -> Result<String> {
    find_tool_policy(name)
        .map(|policy| policy.canonical_name.to_string())
        .ok_or_else(|| anyhow!(unsupported_tool_message(name)))
}

fn unsupported_tool_message(name: &str) -> String {
    match closest_supported_tool_name(name) {
        Some(suggestion) => format!("unsupported tool `{name}`; did you mean `{suggestion}`?"),
        None => format!("unsupported tool `{name}`; run `za tool ls --supported` to list tools"),
    }
}

fn closest_supported_tool_name(name: &str) -> Option<&'static str> {
    let needle = name.to_ascii_lowercase();
    let mut best = None::<(&'static str, usize)>;

    for policy in tool_policies() {
        for candidate in policy.supported_names() {
            let distance = levenshtein_ascii(&needle, &candidate.to_ascii_lowercase());
            if best.is_none_or(|(_, best_distance)| distance < best_distance) {
                best = Some((candidate, distance));
            }
        }
    }

    let threshold = match needle.len() {
        0..=4 => 1,
        5..=8 => 2,
        _ => 3,
    };
    best.and_then(|(candidate, distance)| (distance <= threshold).then_some(candidate))
}

fn levenshtein_ascii(left: &str, right: &str) -> usize {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let mut prev = (0..=right.len()).collect::<Vec<_>>();
    let mut curr = vec![0; right.len() + 1];

    for (i, left_byte) in left.iter().enumerate() {
        curr[0] = i + 1;
        for (j, right_byte) in right.iter().enumerate() {
            let cost = usize::from(left_byte != right_byte);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[right.len()]
}

fn tool_layout_for_name(name: &str) -> ToolLayout {
    find_tool_policy(name)
        .map(|policy| policy.layout)
        .unwrap_or(ToolLayout::Binary)
}

fn package_policy_for_name(name: &str) -> Option<PackagePolicy> {
    find_tool_policy(name).and_then(|policy| policy.package)
}

fn detect_adoption_candidate(
    home: &ToolHome,
    requested: &ToolSpec,
) -> Result<Option<AdoptionCandidate>> {
    if tool_layout_for_name(&requested.name) == ToolLayout::Package {
        bail!(
            "`{}` uses a package-style install and cannot be adopted from an unmanaged binary",
            requested.name
        );
    }
    if requested.version.is_some() {
        return Ok(None);
    }

    let Some(bin_path) = find_existing_executable_for_name(home, &requested.name) else {
        return Ok(None);
    };
    let Some(version) = probe_binary_version(&bin_path)? else {
        return Ok(None);
    };

    Ok(Some(AdoptionCandidate {
        path: bin_path,
        version,
    }))
}

fn is_name_managed(home: &ToolHome, name: &str) -> Result<bool> {
    Ok(!collect_dir_names(&home.name_dir(name))?.is_empty())
}

fn find_existing_executable(home: &ToolHome, name: &str) -> Option<PathBuf> {
    for candidate in command_candidates(name) {
        let path = home.bin_path(&candidate);
        if is_executable_file(&path) {
            return Some(path);
        }
    }
    None
}

fn is_policy_managed(home: &ToolHome, policy: ToolPolicy) -> Result<bool> {
    for name in policy.supported_names() {
        if is_name_managed(home, name)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn find_existing_executable_for_name(home: &ToolHome, name: &str) -> Option<PathBuf> {
    let Some(policy) = find_tool_policy(name) else {
        return find_existing_executable(home, name);
    };
    for supported_name in policy.supported_names() {
        if let Some(path) = find_existing_executable(home, supported_name) {
            return Some(path);
        }
    }
    None
}

fn collect_unmanaged_binaries(home: &ToolHome) -> Result<Vec<UnmanagedBinary>> {
    let mut out = Vec::new();
    for policy in tool_policies() {
        if policy.layout != ToolLayout::Binary {
            continue;
        }
        if is_policy_managed(home, *policy)? {
            continue;
        }
        let Some(path) = find_existing_executable_for_name(home, policy.canonical_name) else {
            continue;
        };
        let version = probe_binary_version(&path)?.unwrap_or_else(|| "unknown".to_string());
        out.push(UnmanagedBinary {
            name: policy.canonical_name.to_string(),
            version,
            path: path.display().to_string(),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

fn probe_binary_version(binary_path: &Path) -> Result<Option<String>> {
    let output = match Command::new(binary_path).arg("--version").output() {
        Ok(output) => output,
        Err(_) => return Ok(None),
    };
    if !output.status.success() {
        return Ok(None);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let merged = format!("{stdout}\n{stderr}");
    Ok(extract_version_from_text(&merged))
}

fn extract_version_from_text(text: &str) -> Option<String> {
    let caps = VERSION_RE.captures(text)?;
    let version = caps
        .get(1)
        .map(|m| normalize_version(m.as_str()))
        .unwrap_or_default();
    if version.is_empty() {
        return None;
    }
    Some(version)
}

fn create_dir_all_with_context(path: &Path, label: &str) -> Result<()> {
    fs::create_dir_all(path).map_err(|err| {
        if err.kind() == io::ErrorKind::PermissionDenied {
            anyhow!(
                "permission denied creating {label} directory: {}",
                path.display()
            )
        } else {
            anyhow!(
                "failed to create {label} directory {}: {err}",
                path.display()
            )
        }
    })
}

fn is_permission_denied_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|ioe| ioe.kind() == io::ErrorKind::PermissionDenied)
    })
}

#[cfg(test)]
mod tests;
