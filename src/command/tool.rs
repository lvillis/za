//! Tool manager for versioned executables.

mod listing;
mod policy;
mod source;

use anyhow::{Context, Result, anyhow, bail};
use flate2::read::GzDecoder;
use fs4::fs_std::FileExt;
use regex::Regex;
use reqx::{
    RedirectPolicy, RetryPolicy,
    blocking::{Client, ClientBuilder},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    env,
    fs::{self, File, OpenOptions},
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, LazyLock, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tar::Archive;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[cfg(test)]
use self::listing::{LatestCheck, list_update_status};
use self::listing::{UnmanagedBinary, list};
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
const MANIFEST_SCHEMA_VERSION: u32 = 1;
const SOURCE_KIND_DOWNLOAD: &str = "download";
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

pub fn run(cmd: ToolCommands, user: bool) -> Result<i32> {
    let scope = ToolScope::from_flags(user);
    let home = ToolHome::detect(scope)?;

    if let ToolCommands::List {
        supported,
        updates,
        json,
        fail_on_updates,
        fail_on_check_errors,
    } = cmd
    {
        return list(
            &home,
            supported,
            updates,
            json,
            fail_on_updates,
            fail_on_check_errors,
        );
    }

    if let Err(err) = home.ensure_layout() {
        if scope == ToolScope::Global {
            return Err(err).with_context(|| {
                "cannot initialize global tool directories. retry with `za tool --user ...` or run with elevated privileges"
                    .to_string()
            });
        }
        return Err(err);
    }
    let _lock = match ToolLock::acquire(&home) {
        Ok(lock) => lock,
        Err(err) if scope == ToolScope::Global => {
            return Err(err).with_context(|| {
                "cannot acquire global tool lock. retry with `za tool --user ...` or run with elevated privileges"
                    .to_string()
            });
        }
        Err(err) => return Err(err),
    };

    match cmd {
        ToolCommands::Install { spec } => {
            let _ = install(&home, &spec, ToolAction::Install, false)?;
        }
        ToolCommands::Update { spec } => {
            let _ = install(&home, &spec, ToolAction::Update, true)?;
        }
        ToolCommands::Sync { file } => sync_manifest(&home, &file)?,
        ToolCommands::Use { image } => use_tool(&home, &image)?,
        ToolCommands::Uninstall { spec } => uninstall(&home, &spec)?,
        ToolCommands::List { .. } => unreachable!("list handled before mutable operations"),
    };

    Ok(0)
}

pub fn update_self(user: bool, check: bool, version: Option<String>) -> Result<i32> {
    let scope = ToolScope::from_flags(user);
    let home = ToolHome::detect(scope)?;

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
    let target_version = resolve_requested_version("za", requested)?;
    let target_spec = format!("za:{target_version}");
    let previous_active = read_current_version(&home, "za")?;
    let backup = backup_existing_self_binary(&home)?;

    let installed = install(&home, &target_spec, ToolAction::Update, false)?;
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
        println!("🧹 Removed old versions for `za`: {}", removed.join(", "));
    }
    println!("✅ Self-update complete: {}", installed.image());
    Ok(0)
}

fn check_self_update(requested_version: &Option<String>) -> Result<i32> {
    let current = normalize_version(env!("CARGO_PKG_VERSION"));
    let target = resolve_requested_version("za", requested_version.as_deref())?;

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

    fs::create_dir_all(&home.current_dir)?;
    let backup = home
        .current_dir
        .join(format!("za-self-backup-{}", std::process::id()));
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

#[derive(Debug)]
struct PullSource {
    path: PathBuf,
    resolved_by: String,
    cleanup_root: Option<PathBuf>,
}

impl PullSource {
    fn temp(path: PathBuf, resolved_by: String, cleanup_root: PathBuf) -> Self {
        Self {
            path,
            resolved_by,
            cleanup_root: Some(cleanup_root),
        }
    }
}

impl Drop for PullSource {
    fn drop(&mut self) {
        if let Some(root) = &self.cleanup_root {
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
    fn parse(input: &str) -> Result<Self> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            bail!("tool spec must not be empty");
        }
        let (name, version) = if let Some((n, v)) = trimmed.split_once(':') {
            (n, Some(v))
        } else {
            (trimmed, None)
        };
        validate_name(name)?;
        let version = version.map(str::trim).filter(|v| !v.is_empty());
        Ok(Self {
            name: name.to_string(),
            version: version.map(ToOwned::to_owned),
        })
    }

    fn resolve(self, resolved_version: String) -> ToolRef {
        ToolRef {
            name: self.name,
            version: resolved_version,
        }
    }
}

impl ToolRef {
    fn parse(input: &str) -> Result<Self> {
        let (name, version) = input
            .split_once(':')
            .ok_or_else(|| anyhow!("invalid tool ref `{input}`: expected `name:version`"))?;
        validate_name(name)?;
        if version.trim().is_empty() {
            bail!("invalid tool ref `{input}`: version must not be empty");
        }
        Ok(Self {
            name: name.to_string(),
            version: version.to_string(),
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

#[derive(Debug)]
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

fn install(
    home: &ToolHome,
    spec: &str,
    action: ToolAction,
    prune_after_update_activation: bool,
) -> Result<ToolRef> {
    let mut requested = ToolSpec::parse(spec)?;
    requested.name = canonical_tool_name(&requested.name);
    let adoption = if action == ToolAction::Update {
        None
    } else {
        detect_adoption_candidate(home, &requested)?
    };
    let version = if let Some(v) = requested.version.as_deref() {
        let v = normalize_version(v);
        if v.is_empty() {
            bail!("version must not be empty");
        }
        v
    } else if let Some(adopted) = adoption.as_ref() {
        adopted.version.clone()
    } else if action == ToolAction::Update {
        println!("🔎 Resolving latest release for `{}`...", requested.name);
        resolve_requested_version(&requested.name, None)?
    } else {
        resolve_requested_version(&requested.name, None)?
    };
    let tool = requested.resolve(version);
    let previous_active = read_current_version(home, &tool.name)?;
    let dst = home.install_path(&tool);
    let already_installed = dst.exists();

    if action == ToolAction::Update {
        match previous_active.as_deref() {
            Some(current)
                if normalize_version(current) == normalize_version(&tool.version)
                    && already_installed =>
            {
                println!(
                    "✅ `{}` is already up-to-date at {}",
                    tool.name, tool.version
                );
            }
            Some(current) => println!(
                "⬆️  Updating `{}`: {} -> {}",
                tool.name, current, tool.version
            ),
            None => println!("⬆️  Updating `{}` to {}", tool.name, tool.version),
        }
    }

    if !already_installed {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }

        let source = if let Some(adopted) = adoption.filter(|a| a.version == tool.version) {
            copy_executable(&adopted.path, &dst)?;
            InstallSource {
                kind: SOURCE_KIND_ADOPTED,
                detail: format!("existing binary {}", adopted.path.display()),
            }
        } else {
            if action == ToolAction::Update {
                println!("⬇️  Downloading `{}` {} ...", tool.name, tool.version);
            }
            let src = resolve_install_source(&tool)?;
            copy_executable(&src.path, &dst)?;
            InstallSource {
                kind: SOURCE_KIND_DOWNLOAD,
                detail: src.resolved_by.clone(),
            }
        };
        write_manifest(home, &tool, &source)?;
        println!("📥 Installed {} from {}", tool.image(), source.detail);
    } else {
        ensure_manifest(home, &tool)?;
        println!("📦 Already installed: {}", tool.image());
    }

    let should_activate = action == ToolAction::Update || previous_active.is_none();
    if should_activate {
        activate_tool(home, &tool)?;
        println!(
            "✅ Active version set: {} (bin: {})",
            tool.image(),
            home.bin_path(&tool.name).display()
        );
        if action == ToolAction::Update && prune_after_update_activation {
            let removed = prune_non_active_versions(home, &tool)?;
            if !removed.is_empty() {
                println!(
                    "🧹 Removed old versions for `{}`: {}",
                    tool.name,
                    removed.join(", ")
                );
            }
        }
    } else if !already_installed {
        println!("ℹ️  Run `za tool use {}` to activate it.", tool.image());
    }

    Ok(tool)
}

fn sync_manifest(home: &ToolHome, file: &Path) -> Result<()> {
    let specs = load_sync_specs_from_manifest(file)?;
    println!("🔄 Syncing {} tool(s) from {}", specs.len(), file.display());

    let mut failures = Vec::new();
    for (idx, spec) in specs.iter().enumerate() {
        println!("➡️  [{}/{}] {}", idx + 1, specs.len(), spec);
        if let Err(err) = install(home, spec, ToolAction::Update, true) {
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

    if let Some(policy) = find_tool_policy(&requested.name)
        && is_policy_managed(home, policy)?
    {
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

fn use_tool(home: &ToolHome, image: &str) -> Result<()> {
    let mut tool = ToolRef::parse(image)?;
    tool.name = canonical_tool_name(&tool.name);
    let target = home.install_path(&tool);
    if !target.exists() {
        bail!("tool version not installed: {}", tool.image());
    }

    activate_tool(home, &tool)?;
    println!(
        "✅ Using {} (bin: {})",
        tool.image(),
        home.bin_path(&tool.name).display()
    );
    Ok(())
}

fn uninstall(home: &ToolHome, spec: &str) -> Result<()> {
    let mut requested = ToolSpec::parse(spec)?;
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

    let tmp = dst.with_extension("tmp");
    fs::copy(src, &tmp).with_context(|| format!("copy {} -> {}", src.display(), tmp.display()))?;

    #[cfg(unix)]
    {
        let src_mode = fs::metadata(src)?.permissions().mode();
        let mode = src_mode | 0o111;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    }

    if dst.exists() {
        fs::remove_file(dst)?;
    }
    fs::rename(&tmp, dst)?;
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
    fs::rename(&tmp, &p).with_context(|| {
        format!(
            "replace current version {} -> {}",
            p.display(),
            tmp.display()
        )
    })?;
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
    fs::rename(&tmp, dst)
        .with_context(|| format!("activate link {} -> {}", dst.display(), src.display()))
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
