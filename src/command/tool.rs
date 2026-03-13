//! Tool manager for versioned executables.

mod listing;
mod policy;
mod source;

use anyhow::{Context, Result, anyhow, bail};
use flate2::read::GzDecoder;
use fs4::fs_std::FileExt;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use regex::Regex;
use reqx::{
    advanced::{ClientProfile, RedirectPolicy},
    blocking::{Client, ClientBuilder},
    prelude::RetryPolicy,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use signal_hook::{consts::signal::SIGINT, flag as signal_flag};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    env,
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
use tar::Archive;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[cfg(test)]
use self::listing::{LatestCheck, latest_check_progress_message, list_update_status};
use self::listing::{UnmanagedBinary, list_installed, list_outdated, show_catalog, show_tool};
use self::policy::{
    GithubReleasePolicy, ToolPolicy, canonical_tool_name as canonical_tool_name_impl,
    find_tool_policy, supported_tool_names_csv, tool_policies,
};
use self::source::{resolve_install_source, resolve_requested_version};
use crate::{cli::ToolCommands, command::za_config};

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
const SOURCE_KIND_CARGO_INSTALL: &str = "cargo-install";
const SOURCE_KIND_ADOPTED: &str = "adopted";
const SOURCE_KIND_SYNTHESIZED: &str = "synthesized";
const PROXY_HINT: &str =
    "if your network requires a proxy, set HTTPS_PROXY/HTTP_PROXY (and optional NO_PROXY)";
const TOOL_UPDATE_CACHE_SCHEMA_VERSION: u32 = 1;
const TOOL_UPDATE_CACHE_FILE_NAME: &str = "tool-latest-cache-v1.json";
const TOOL_UPDATE_CACHE_TTL_SECS: u64 = 10 * 60;
const TOOL_UPDATE_JOBS_MULTIPLIER: usize = 2;
const TOOL_UPDATE_JOBS_MIN: usize = 2;
const TOOL_UPDATE_JOBS_MAX: usize = 8;
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
    if io::stdout().is_terminal() {
        println!("{} {stage:<8} {}", tool_stage_icon(stage), message.as_ref());
    } else {
        println!("{stage:<8} {}", message.as_ref());
    }
}

fn tool_stage_icon(stage: &str) -> &'static str {
    match stage {
        "resolve" => "🔎",
        "update" => "⬆️",
        "source" => "📦",
        "install" => "📥",
        "activate" => "✅",
        "prune" => "🧹",
        "next" => "ℹ️",
        "done" => "✅",
        _ => "•",
    }
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

impl ToolScope {
    fn from_flags(user: bool) -> Self {
        if user { Self::User } else { Self::Global }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::User => "user",
        }
    }
}

fn ensure_tool_home_ready(home: &ToolHome, scope: ToolScope) -> Result<ToolLock> {
    if let Err(err) = home.ensure_layout() {
        if scope == ToolScope::Global {
            return Err(err).with_context(|| {
                "cannot initialize global tool directories. retry with `za tool --user ...` or run with elevated privileges"
                    .to_string()
            });
        }
        return Err(err);
    }
    match ToolLock::acquire(home) {
        Ok(lock) => Ok(lock),
        Err(err) if scope == ToolScope::Global => Err(err).with_context(|| {
            "cannot acquire global tool lock. retry with `za tool --user ...` or run with elevated privileges"
                .to_string()
        }),
        Err(err) => Err(err),
    }
}

fn run_mutating_tool_command<F>(home: &ToolHome, scope: ToolScope, action: F) -> Result<i32>
where
    F: FnOnce() -> Result<()>,
{
    let _lock = ensure_tool_home_ready(home, scope)?;
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

pub fn run(cmd: ToolCommands, user: bool) -> Result<i32> {
    prepare_interruptible_tool_operation()?;

    let scope = ToolScope::from_flags(user);
    let home = ToolHome::detect(scope)?;
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
            tool,
            version,
            adopt,
        } => {
            let home_for_action = home.clone();
            run_mutating_tool_command(&home, scope, move || {
                install_tools(
                    &home_for_action,
                    std::slice::from_ref(&tool),
                    version.as_deref(),
                    adopt,
                )
            })
        }
        ToolCommands::Update { tools, version } => {
            let home_for_action = home.clone();
            run_mutating_tool_command(&home, scope, move || {
                install_tools(&home_for_action, &tools, version.as_deref(), false)
            })
        }
        ToolCommands::Sync { file } => {
            let home_for_action = home.clone();
            run_mutating_tool_command(&home, scope, move || sync_manifest(&home_for_action, &file))
        }
        ToolCommands::Uninstall { tool, version } => {
            let home_for_action = home.clone();
            run_mutating_tool_command(&home, scope, move || {
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
            run_mutating_tool_command(&home, scope, move || {
                install_tools(&home_for_action, std::slice::from_ref(&tool), None, true)
            })
        }
    }
}

pub fn update_self(user: bool, check: bool, version: Option<String>) -> Result<i32> {
    prepare_interruptible_tool_operation()?;

    let scope = ToolScope::from_flags(user);
    let home = ToolHome::detect(scope)?;
    cleanup_legacy_current_dir_artifacts(&home)?;

    if check {
        return check_self_update(&version);
    }

    if let Err(err) = home.ensure_layout() {
        if scope == ToolScope::Global {
            return Err(err).with_context(|| {
                "cannot initialize global tool directories. retry with `za update --user` or run with elevated privileges"
                    .to_string()
            });
        }
        return Err(err);
    }
    let _lock = match ToolLock::acquire(&home) {
        Ok(lock) => lock,
        Err(err) if scope == ToolScope::Global => {
            return Err(err).with_context(|| {
                "cannot acquire global tool lock. retry with `za update --user` or run with elevated privileges"
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
    if let Err(err) = verify_self_update(&home, &installed) {
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
    let removed = prune_non_active_versions(&home, &installed)?;
    if !removed.is_empty() {
        print_tool_stage(
            "prune",
            format!("removed old `za` versions: {}", removed.join(", ")),
        );
    }
    print_tool_stage(
        "done",
        format!("self-update complete: {}", installed.image()),
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
}

impl InstallOptions {
    fn update(proxy_scope: za_config::ProxyScope) -> Self {
        Self {
            action: ToolAction::Update,
            adoption: AdoptionMode::Disallow,
            prune_after_activation: true,
            proxy_scope,
        }
    }

    fn adopt(proxy_scope: za_config::ProxyScope) -> Self {
        Self {
            action: ToolAction::Install,
            adoption: AdoptionMode::Require,
            prune_after_activation: false,
            proxy_scope,
        }
    }

    fn with_prune(mut self, prune_after_activation: bool) -> Self {
        self.prune_after_activation = prune_after_activation;
        self
    }
}

#[derive(Debug)]
struct PullSource {
    kind: &'static str,
    path: PathBuf,
    resolved_by: String,
    cleanup_root: Option<PathBuf>,
}

impl PullSource {
    fn temp(kind: &'static str, path: PathBuf, resolved_by: String, cleanup_root: PathBuf) -> Self {
        Self {
            kind,
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
    fn detect(scope: ToolScope) -> Result<Self> {
        match scope {
            ToolScope::Global => Ok(Self {
                scope,
                store_dir: PathBuf::from("/var/lib/za/tools/store"),
                current_dir: PathBuf::from("/var/lib/za/tools/current"),
                bin_dir: PathBuf::from("/usr/local/bin"),
            }),
            ToolScope::User => {
                let home = env::var_os("HOME")
                    .map(PathBuf::from)
                    .ok_or_else(|| anyhow!("cannot resolve user paths: set `HOME`"))?;

                let data_home = env::var_os("XDG_DATA_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(".local/share"));
                let state_home = env::var_os("XDG_STATE_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(".local/state"));
                let bin_home = env::var_os("XDG_BIN_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(".local/bin"));

                Ok(Self {
                    scope,
                    store_dir: data_home.join("za/tools/store"),
                    current_dir: state_home.join("za/tools/current"),
                    bin_dir: bin_home,
                })
            }
        }
    }

    fn ensure_layout(&self) -> Result<()> {
        create_dir_all_with_context(&self.store_dir, "store")?;
        create_dir_all_with_context(&self.current_dir, "current")?;
        create_dir_all_with_context(&self.bin_dir, "bin")?;
        Ok(())
    }

    fn install_path(&self, tool: &ToolRef) -> PathBuf {
        self.store_dir
            .join(&tool.name)
            .join(&tool.version)
            .join(&tool.name)
    }

    fn version_dir(&self, tool: &ToolRef) -> PathBuf {
        self.store_dir.join(&tool.name).join(&tool.version)
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

    fn bin_path(&self, name: &str) -> PathBuf {
        self.bin_dir.join(name)
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
        file.lock_exclusive()
            .with_context(|| format!("acquire lock {}", lock_path.display()))?;
        Ok(Self { _file: file })
    }
}

impl Drop for ToolLock {
    fn drop(&mut self) {
        let _ = self._file.unlock();
    }
}

fn install(home: &ToolHome, mut requested: ToolSpec, options: InstallOptions) -> Result<ToolRef> {
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
        print_tool_stage(
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

    if options.action == ToolAction::Update {
        match previous_active.as_deref() {
            Some(current)
                if normalize_version(current) == normalize_version(&tool.version)
                    && already_installed =>
            {
                print_tool_stage(
                    "update",
                    format!("`{}` already at {}", tool.name, tool.version),
                );
            }
            Some(current) => print_tool_stage(
                "update",
                format!("`{}` {} -> {}", tool.name, current, tool.version),
            ),
            None => print_tool_stage("update", format!("`{}` -> {}", tool.name, tool.version)),
        }
    }

    if !already_installed {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }

        let source = if let Some(adopted) = adoption.filter(|a| a.version == tool.version) {
            print_tool_stage(
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
            print_tool_stage(
                "source",
                format!("fetching `{}` {}", tool.name, tool.version),
            );
            let src = match resolve_install_source(&tool, options.proxy_scope) {
                Ok(src) => src,
                Err(err) => {
                    return Err(match install_source_failure_guidance(&err) {
                        Some(guidance) => err.context(guidance),
                        None => err,
                    });
                }
            };
            ensure_not_interrupted()?;
            copy_executable(&src.path, &dst)?;
            InstallSource {
                kind: src.kind,
                detail: src.resolved_by.clone(),
            }
        };
        write_manifest(home, &tool, &source)?;
        print_tool_stage(
            "install",
            format!("{} from {}", tool.image(), source.detail),
        );
    } else {
        ensure_manifest(home, &tool)?;
        print_tool_stage("install", format!("already installed {}", tool.image()));
    }

    activate_tool(home, &tool)?;
    print_tool_stage(
        "activate",
        format!(
            "{} (bin: {})",
            tool.image(),
            home.bin_path(&tool.name).display()
        ),
    );
    if options.prune_after_activation {
        let removed = prune_non_active_versions(home, &tool)?;
        if !removed.is_empty() {
            print_tool_stage(
                "prune",
                format!(
                    "removed old `{}` versions: {}",
                    tool.name,
                    removed.join(", ")
                ),
            );
        }
    }

    Ok(tool)
}

fn install_tools(
    home: &ToolHome,
    tools: &[String],
    version: Option<&str>,
    adopt: bool,
) -> Result<()> {
    if adopt && version.is_some() {
        bail!("`za tool install --adopt` does not accept `--version`");
    }
    if adopt && tools.len() != 1 {
        bail!("`za tool install --adopt` requires exactly one tool name");
    }
    if version.is_some() && tools.len() != 1 {
        bail!("`za tool update --version` requires exactly one tool name");
    }

    let requested_names = if tools.is_empty() {
        if adopt {
            bail!("`za tool install --adopt` requires a tool name");
        }
        collect_managed_tool_names(home)?
    } else {
        normalize_requested_tool_names(tools)?
    };

    if requested_names.is_empty() {
        println!(
            "No managed tools installed in {} scope.",
            home.scope.label()
        );
        return Ok(());
    }

    let total = requested_names.len();
    for (idx, name) in requested_names.iter().enumerate() {
        if total > 1 {
            println!("➡️  [{}/{}] {}", idx + 1, total, name);
        }
        if adopt {
            adopt_tool(home, name)?;
        } else {
            let _ = install(
                home,
                ToolSpec::from_args(name, version)?,
                InstallOptions::update(za_config::ProxyScope::Tool),
            )?;
        }
    }
    Ok(())
}

fn normalize_requested_tool_names(names: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for name in names {
        let canonical = canonical_tool_name(&ToolSpec::from_args(name, None)?.name);
        if seen.insert(canonical.clone()) {
            out.push(canonical);
        }
    }
    Ok(out)
}

fn sync_manifest(home: &ToolHome, file: &Path) -> Result<()> {
    let specs = load_sync_specs_from_manifest(file)?;
    println!("🔄 Syncing {} tool(s) from {}", specs.len(), file.display());

    let mut failures = Vec::new();
    for (idx, spec) in specs.iter().enumerate() {
        ensure_not_interrupted()?;
        println!("➡️  [{}/{}] {}", idx + 1, specs.len(), spec);
        if let Err(err) = install(
            home,
            ToolSpec::parse(spec)?,
            InstallOptions::update(za_config::ProxyScope::Tool),
        ) {
            failures.push(format!("{spec}: {err:#}"));
        }
    }

    if failures.is_empty() {
        println!("✅ Sync complete: {} tool(s) are up-to-date", specs.len());
        return Ok(());
    }

    bail!(
        "sync completed with {} failure(s):\n- {}",
        failures.len(),
        failures.join("\n- ")
    )
}

pub(super) fn load_sync_specs_from_manifest(file: &Path) -> Result<Vec<String>> {
    let raw = fs::read_to_string(file)
        .with_context(|| format!("read sync manifest {}", file.display()))?;
    let manifest = toml::from_str::<ToolSyncManifest>(&raw)
        .with_context(|| format!("parse sync manifest {}", file.display()))?;
    if manifest.tools.is_empty() {
        bail!(
            "sync manifest {} has no tools; expected `tools = [\"codex\", \"docker-compose\"]`",
            file.display()
        );
    }

    let mut specs = Vec::new();
    let mut seen = HashSet::new();
    for raw_spec in manifest.tools {
        let trimmed = raw_spec.trim();
        if trimmed.is_empty() {
            bail!(
                "sync manifest {} contains an empty tool spec",
                file.display()
            );
        }

        let mut parsed = ToolSpec::parse(trimmed)
            .with_context(|| format!("invalid tool spec `{trimmed}` in {}", file.display()))?;
        parsed.name = canonical_tool_name(&parsed.name);
        let normalized = match parsed.version {
            Some(version) => format!("{}:{}", parsed.name, normalize_version(&version)),
            None => parsed.name,
        };
        if seen.insert(normalized.clone()) {
            specs.push(normalized);
        }
    }

    if specs.is_empty() {
        bail!(
            "sync manifest {} has no valid tools after normalization",
            file.display()
        );
    }
    Ok(specs)
}

fn normalize_version(version: &str) -> String {
    version.trim_start_matches('v').to_string()
}

pub(crate) fn canonical_tool_name(name: &str) -> String {
    canonical_tool_name_impl(name)
}

fn detect_adoption_candidate(
    home: &ToolHome,
    requested: &ToolSpec,
) -> Result<Option<AdoptionCandidate>> {
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

fn write_manifest(home: &ToolHome, tool: &ToolRef, source: &InstallSource) -> Result<()> {
    let install_path = home.install_path(tool);
    let meta = fs::metadata(&install_path)
        .with_context(|| format!("stat installed executable {}", install_path.display()))?;
    let digest = sha256_file(&install_path)?;
    let manifest = ToolManifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        name: tool.name.clone(),
        version: tool.version.clone(),
        installed_at_unix_secs: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        source_kind: source.kind.to_string(),
        source_detail: source.detail.clone(),
        sha256: digest,
        size_bytes: meta.len(),
    };

    let manifest_path = home.manifest_path(tool);
    if let Some(parent) = manifest_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_vec_pretty(&manifest).context("serialize tool manifest")?;
    fs::write(&manifest_path, content)
        .with_context(|| format!("write manifest {}", manifest_path.display()))?;
    Ok(())
}

fn ensure_manifest(home: &ToolHome, tool: &ToolRef) -> Result<()> {
    let manifest_path = home.manifest_path(tool);
    if manifest_path.exists() {
        return Ok(());
    }
    let source = InstallSource {
        kind: SOURCE_KIND_SYNTHESIZED,
        detail: "legacy install inferred from store layout".to_string(),
    };
    write_manifest(home, tool, &source)
}

fn manifest_source_label(home: &ToolHome, tool: &ToolRef) -> Result<String> {
    let manifest_path = home.manifest_path(tool);
    if !manifest_path.exists() {
        return Ok("unknown".to_string());
    }
    let raw = match fs::read_to_string(&manifest_path) {
        Ok(raw) => raw,
        Err(_) => return Ok("unreadable".to_string()),
    };
    match serde_json::from_str::<ToolManifest>(&raw) {
        Ok(manifest) => Ok(manifest.source_kind),
        Err(_) => Ok("invalid".to_string()),
    }
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("read {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn adopt_tool(home: &ToolHome, tool: &str) -> Result<()> {
    let mut requested = ToolSpec::from_args(tool, None)?;
    requested.name = canonical_tool_name(&requested.name);
    if is_name_managed(home, &requested.name)? {
        bail!(
            "`{}` is already managed in {} scope; use `za tool update {}` to refresh it",
            requested.name,
            home.scope.label(),
            requested.name
        );
    }

    let installed = install(
        home,
        requested,
        InstallOptions::adopt(za_config::ProxyScope::Tool),
    )?;
    println!("✅ Adopted {}", installed.image());
    Ok(())
}

fn uninstall(home: &ToolHome, mut requested: ToolSpec) -> Result<()> {
    requested.name = canonical_tool_name(&requested.name);
    match requested.version {
        Some(version) => uninstall_version(
            home,
            &ToolRef {
                name: requested.name,
                version: normalize_version(&version),
            },
        ),
        None => uninstall_all_versions(home, &requested.name),
    }
}

fn uninstall_version(home: &ToolHome, tool: &ToolRef) -> Result<()> {
    let version_dir = home.version_dir(tool);
    if !version_dir.exists() {
        println!("🗑  Not installed: {}", tool.image());
        return Ok(());
    }

    let was_current = read_current_version(home, &tool.name)?
        .as_deref()
        .is_some_and(|v| v == tool.version);
    fs::remove_dir_all(&version_dir)
        .with_context(|| format!("remove {}", version_dir.display()))?;

    if collect_dir_names(&home.name_dir(&tool.name))?.is_empty() {
        let _ = fs::remove_dir(home.name_dir(&tool.name));
    }

    if was_current {
        remove_file_if_exists(&home.current_file(&tool.name))?;
        remove_file_if_exists(&home.bin_path(&tool.name))?;
        println!("🗑  Removed {} and cleared active version", tool.image());
    } else {
        println!("🗑  Removed {}", tool.image());
    }

    Ok(())
}

fn uninstall_all_versions(home: &ToolHome, name: &str) -> Result<()> {
    validate_name(name)?;
    let name_dir = home.name_dir(name);
    if !name_dir.exists() {
        println!("🗑  Not installed: {name}");
        return Ok(());
    }

    let versions = collect_dir_names(&name_dir)?;
    let removed_count = versions.len();
    fs::remove_dir_all(&name_dir).with_context(|| format!("remove {}", name_dir.display()))?;
    remove_file_if_exists(&home.current_file(name))?;
    remove_file_if_exists(&home.bin_path(name))?;

    println!("🗑  Removed {name} ({removed_count} version(s)) and cleared active entry");
    Ok(())
}

fn prune_non_active_versions(home: &ToolHome, active: &ToolRef) -> Result<Vec<String>> {
    let name_dir = home.name_dir(&active.name);
    if !name_dir.exists() {
        return Ok(Vec::new());
    }

    let active_version = normalize_version(&active.version);
    let mut removed = Vec::new();
    for version in collect_dir_names(&name_dir)? {
        if normalize_version(&version) == active_version {
            continue;
        }
        let stale = ToolRef {
            name: active.name.clone(),
            version: version.clone(),
        };
        let stale_dir = home.version_dir(&stale);
        if stale_dir.exists() {
            fs::remove_dir_all(&stale_dir)
                .with_context(|| format!("remove stale version {}", stale_dir.display()))?;
            removed.push(version);
        }
    }
    removed.sort();
    Ok(removed)
}

fn command_candidates(name: &str) -> Vec<String> {
    let mut out = vec![name.to_string()];
    if let Some(stripped) = name.strip_suffix("-cli")
        && !stripped.is_empty()
    {
        out.push(stripped.to_string());
    }
    if let Some(stripped) = name.strip_suffix("_cli")
        && !stripped.is_empty()
    {
        out.push(stripped.to_string());
    }
    out.sort();
    out.dedup();
    out
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn copy_executable(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }

    let tmp = dst.with_extension(format!("tmp-copy-{}", std::process::id()));
    remove_file_if_exists(&tmp)?;
    fs::copy(src, &tmp).with_context(|| format!("copy {} -> {}", src.display(), tmp.display()))?;

    #[cfg(unix)]
    {
        let src_mode = fs::metadata(src)?.permissions().mode();
        let mode = src_mode | 0o111;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    }

    if let Err(err) = fs::rename(&tmp, dst) {
        #[cfg(windows)]
        {
            if err.kind() == io::ErrorKind::AlreadyExists {
                remove_file_if_exists(dst)?;
                fs::rename(&tmp, dst).with_context(|| {
                    format!(
                        "replace executable {} with {}",
                        dst.display(),
                        tmp.display()
                    )
                })?;
            } else {
                let _ = remove_file_if_exists(&tmp);
                return Err(err)
                    .with_context(|| format!("rename {} -> {}", tmp.display(), dst.display()));
            }
        }
        #[cfg(not(windows))]
        {
            let _ = remove_file_if_exists(&tmp);
            return Err(err)
                .with_context(|| format!("rename {} -> {}", tmp.display(), dst.display()));
        }
    }
    Ok(())
}

fn read_current_version(home: &ToolHome, name: &str) -> Result<Option<String>> {
    let p = home.current_file(name);
    if !p.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(&p)
        .with_context(|| format!("read current version file {}", p.display()))?;
    let version = content.trim().to_string();
    if version.is_empty() {
        return Ok(None);
    }
    Ok(Some(version))
}

fn print_active_managed_path(home: &ToolHome, tool: &str) -> Result<()> {
    let name = canonical_tool_name(&ToolSpec::from_args(tool, None)?.name);
    let Some(_version) = read_current_version(home, &name)? else {
        bail!(
            "`{}` has no active managed version in {} scope",
            name,
            home.scope.label()
        );
    };
    let bin = home.bin_path(&name);
    if !bin.exists() {
        bail!(
            "active managed binary for `{}` is missing at {}; repair with `za tool update {}`",
            name,
            bin.display(),
            name
        );
    }
    println!("{}", bin.display());
    Ok(())
}

fn activate_tool(home: &ToolHome, tool: &ToolRef) -> Result<()> {
    let previous_active = read_current_version(home, &tool.name)?;
    sync_bin_entry(home, tool)?;

    if let Err(err) = set_current_version(home, tool) {
        let restore_res = restore_bin_entry(home, &tool.name, previous_active.as_deref());
        let err = err.context("persist active tool version");
        if let Err(restore_err) = restore_res {
            return Err(err.context(format!("rollback bin entry failed: {restore_err}")));
        }
        return Err(err);
    }

    Ok(())
}

fn restore_bin_entry(home: &ToolHome, name: &str, previous_version: Option<&str>) -> Result<()> {
    match previous_version {
        Some(version) => {
            let previous = ToolRef {
                name: name.to_string(),
                version: version.to_string(),
            };
            if home.install_path(&previous).exists() {
                sync_bin_entry(home, &previous)?;
            } else {
                remove_file_if_exists(&home.bin_path(name))?;
            }
        }
        None => {
            remove_file_if_exists(&home.bin_path(name))?;
        }
    }
    Ok(())
}

fn set_current_version(home: &ToolHome, tool: &ToolRef) -> Result<()> {
    fs::create_dir_all(&home.current_dir)?;
    let p = home.current_file(&tool.name);
    let tmp = p.with_extension(format!("tmp-current-{}", std::process::id()));
    let mut f = File::create(&tmp).with_context(|| format!("write {}", tmp.display()))?;
    writeln!(f, "{}", tool.version)?;
    f.flush()
        .with_context(|| format!("flush {}", tmp.display()))?;
    if let Err(err) = fs::rename(&tmp, &p) {
        let _ = remove_file_if_exists(&tmp);
        return Err(err).with_context(|| {
            format!(
                "replace current version {} -> {}",
                p.display(),
                tmp.display()
            )
        });
    }
    Ok(())
}

fn sync_bin_entry(home: &ToolHome, tool: &ToolRef) -> Result<()> {
    let src = home.install_path(tool);
    if !src.exists() {
        bail!("tool version not installed: {}", tool.image());
    }
    let dst = home.bin_path(&tool.name);
    if let Err(err) = link_executable(&src, &dst) {
        copy_executable(&src, &dst).with_context(|| {
            format!(
                "activate {} via copy fallback after link failed: {err}",
                tool.image()
            )
        })?;
    }
    Ok(())
}

#[cfg(unix)]
fn link_executable(src: &Path, dst: &Path) -> Result<()> {
    use std::os::unix::fs::symlink;

    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    let src = fs::canonicalize(src).unwrap_or_else(|_| src.to_path_buf());
    let tmp = dst.with_extension(format!("tmp-link-{}", std::process::id()));
    remove_file_if_exists(&tmp)?;
    symlink(&src, &tmp)
        .with_context(|| format!("symlink {} -> {}", tmp.display(), src.display()))?;
    if let Err(err) = fs::rename(&tmp, dst) {
        let _ = remove_file_if_exists(&tmp);
        return Err(err)
            .with_context(|| format!("activate link {} -> {}", dst.display(), src.display()));
    }
    Ok(())
}

#[cfg(not(unix))]
fn link_executable(_src: &Path, _dst: &Path) -> Result<()> {
    bail!("symlink activation is not supported on this platform")
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(_) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn collect_dir_names(root: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    if !root.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            out.push(entry.file_name().to_string_lossy().to_string());
        }
    }
    Ok(out)
}

fn collect_current_state_names(root: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    if !root.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if is_current_state_file_name(&name) {
            out.push(name);
        }
    }
    Ok(out)
}

fn collect_managed_tool_names(home: &ToolHome) -> Result<Vec<String>> {
    let mut names: HashSet<String> = collect_dir_names(&home.store_dir)?.into_iter().collect();
    for file in collect_current_state_names(&home.current_dir)? {
        names.insert(file);
    }
    let mut out = names.into_iter().collect::<Vec<_>>();
    out.sort();
    Ok(out)
}

fn is_current_state_file_name(name: &str) -> bool {
    name != LOCK_FILE
        && !name.starts_with(SELF_UPDATE_BACKUP_PREFIX)
        && !name.contains(CURRENT_TMP_FILE_MARKER)
}

fn cleanup_legacy_current_dir_artifacts(home: &ToolHome) -> Result<()> {
    let mut removed = 0usize;

    removed += cleanup_legacy_files_in_dir(&home.current_dir)?;
    removed += cleanup_legacy_files_in_dir(&home.self_update_backup_dir())?;

    if removed > 0 {
        eprintln!("🧹 Cleaned {removed} legacy tool state artifact(s)");
    }
    Ok(())
}

fn cleanup_legacy_files_in_dir(root: &Path) -> Result<usize> {
    if !root.exists() {
        return Ok(0);
    }

    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => return Ok(0),
        Err(err) => return Err(err).with_context(|| format!("read {}", root.display())),
    };

    let mut removed = 0usize;
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !is_legacy_current_artifact_name(&name) {
            continue;
        }
        match fs::remove_file(entry.path()) {
            Ok(()) => removed += 1,
            Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {}
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "remove legacy tool state artifact {}",
                        entry.path().display()
                    )
                });
            }
        }
    }

    if root.file_name().and_then(|name| name.to_str()) == Some(SELF_UPDATE_BACKUP_DIR) {
        match fs::remove_dir(root) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) if err.kind() == io::ErrorKind::DirectoryNotEmpty => {}
            Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {}
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("remove legacy backup directory {}", root.display()));
            }
        }
    }

    Ok(removed)
}

fn is_legacy_current_artifact_name(name: &str) -> bool {
    name.starts_with(SELF_UPDATE_BACKUP_PREFIX) || name.contains(CURRENT_TMP_FILE_MARKER)
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
