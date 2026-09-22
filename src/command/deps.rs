//! Dependency governance, version drift, and maintenance audit for Rust projects.

mod api;
mod latest;
mod model;
mod render;
pub mod resolve;

use crate::command::{render as text_render, style as tty_style, write_file_atomically, za_config};
use anyhow::{Context, Result, anyhow, bail};
use humantime::format_rfc3339_seconds;
use indicatif::{ProgressBar, ProgressStyle};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    env, fs,
    io::IsTerminal,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, SystemTime},
};

use self::api::ApiClient;
use self::model::{
    ActionAuditRecord, ActionLocation, ActionUpdatePlan, AuditReport, AuditSummary, DepAuditRecord,
    DependencySource, DependencySpec, DependencySpecBuilder, DependencyUpdatePlan,
    GitHubCacheEntry, RiskLevel, age_days_from_now, classify_risk, github_repo_from_url,
    minimum_rust_version, std_alternative,
};
use self::render::{build_summary, print_report, write_json_report};

const HTTP_TIMEOUT_SECS: u64 = 30;
const HTTP_USER_AGENT: &str = "za-deps-audit/0.1";
const HTTP_MAX_ATTEMPTS: usize = 3;
const HTTP_BACKOFF_BASE_MS: u64 = 200;
const AUTO_DEPS_JOBS_MULTIPLIER: usize = 2;
const AUTO_DEPS_JOBS_MIN: usize = 4;
const AUTO_DEPS_JOBS_MAX: usize = 16;
const DEPS_CACHE_SCHEMA_VERSION: u32 = 3;
const DEPS_CACHE_FILE_NAME: &str = "deps-cache-v3.json";
const CRATES_CACHE_TTL_SECS: u64 = 10 * 60;
const GITHUB_CACHE_TTL_SECS: u64 = 60 * 60;
const WORKFLOW_ACTION_REF_MAX_TAGS: usize = 100;
const WORKFLOW_ACTION_REF_MAX_PAGES: usize = 20;

pub struct DepsRunOptions {
    pub manifest_path: Option<PathBuf>,
    pub project_path: Option<PathBuf>,
    pub jobs: Option<usize>,
    pub include_dev: bool,
    pub include_build: bool,
    pub include_optional: bool,
    pub refresh: bool,
    pub json_out: Option<PathBuf>,
    pub fail_on_high: bool,
    pub verbose: bool,
}

pub struct DepsLatestOptions {
    pub manifest_path: Option<PathBuf>,
    pub project_path: Option<PathBuf>,
    pub jobs: Option<usize>,
    pub include_dev: bool,
    pub include_build: bool,
    pub include_optional: bool,
    pub refresh: bool,
    pub json: bool,
    pub toml: bool,
    pub suggest: bool,
}

pub fn run(opts: DepsRunOptions) -> Result<()> {
    let DepsRunOptions {
        manifest_path,
        project_path,
        jobs,
        include_dev,
        include_build,
        include_optional,
        refresh,
        json_out,
        fail_on_high,
        verbose,
    } = opts;

    let manifest_path = resolve_manifest_path(manifest_path, project_path)?;
    let metadata = read_manifest_metadata(&manifest_path)?;
    let project_root = metadata.workspace_root.clone();
    let inventory =
        collect_dependency_inventory(&metadata, include_dev, include_build, include_optional)?;
    let action_specs = collect_workflow_action_specs(&project_root)?;
    if inventory.specs.is_empty() && action_specs.is_empty() {
        let summary = AuditSummary {
            skipped_local: inventory.skipped_local_count(),
            ..AuditSummary::default()
        };
        if let Some(path) = json_out {
            write_json_report(path, &manifest_path, &summary, &[], &[])?;
        }
        if inventory.skipped_local_count() > 0 {
            println!(
                "No external dependencies found for audit; skipped {} internal/path {}.",
                inventory.skipped_local_count(),
                dependency_label(inventory.skipped_local_count())
            );
        } else {
            println!("No dependencies found for audit.");
        }
        return Ok(());
    }

    let requested_jobs = jobs.unwrap_or_else(default_deps_jobs);
    let workload_count = inventory.specs.len() + action_specs.len();
    let worker_count = normalize_jobs(requested_jobs, workload_count);
    println!(
        "Auditing {} dependencies{} with {} workers...",
        inventory.specs.len(),
        render_action_audit_count(action_specs.len()),
        worker_count
    );
    let skipped_local = inventory.skipped_local_count();
    let api = Arc::new(ApiClient::new(refresh)?);
    let mut records = audit_dependencies(Arc::clone(&api), inventory.specs, worker_count)?;
    let mut actions = audit_actions(Arc::clone(&api), action_specs, worker_count)?;
    sort_records(&mut records);
    sort_action_records(&mut actions);

    let summary = build_summary(&records, skipped_local);
    print_report(&manifest_path, &summary, &records, &actions, verbose);

    if let Some(path) = json_out {
        write_json_report(path, &manifest_path, &summary, &records, &actions)?;
    }

    api.flush_cache()?;

    if fail_on_high && summary.high > 0 {
        bail!("dependency audit found {} high-risk entries", summary.high);
    }
    Ok(())
}

pub fn run_latest(opts: DepsLatestOptions) -> Result<()> {
    latest::run_latest(opts)
}

fn normalize_jobs(requested_jobs: usize, deps_count: usize) -> usize {
    requested_jobs.max(1).min(deps_count.max(1))
}

fn default_deps_jobs() -> usize {
    let cpus = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(AUTO_DEPS_JOBS_MIN);
    derive_auto_jobs(cpus)
}

fn derive_auto_jobs(cpu_count: usize) -> usize {
    cpu_count
        .saturating_mul(AUTO_DEPS_JOBS_MULTIPLIER)
        .clamp(AUTO_DEPS_JOBS_MIN, AUTO_DEPS_JOBS_MAX)
}

fn audit_dependencies(
    api: Arc<ApiClient>,
    specs: Vec<DependencySpec>,
    jobs: usize,
) -> Result<Vec<DepAuditRecord>> {
    run_work_queue(
        specs,
        jobs,
        "dependency queue",
        "dependency records",
        |spec| api.audit_one(spec),
    )
}

fn audit_actions(
    api: Arc<ApiClient>,
    specs: Vec<WorkflowActionSpec>,
    jobs: usize,
) -> Result<Vec<ActionAuditRecord>> {
    run_work_queue(
        specs,
        jobs,
        "workflow action queue",
        "workflow action records",
        |spec| api.audit_action(spec),
    )
}

fn run_work_queue<T, R, F>(
    items: Vec<T>,
    jobs: usize,
    queue_label: &'static str,
    records_label: &'static str,
    worker: F,
) -> Result<Vec<R>>
where
    T: Send,
    R: Send,
    F: Fn(T) -> Result<R> + Sync,
{
    let progress = build_progress(items.len() as u64);
    let jobs = normalize_jobs(jobs, items.len());
    let queue = Arc::new(Mutex::new(VecDeque::from(items)));
    let records = Arc::new(Mutex::new(Vec::new()));
    let first_error: Arc<Mutex<Option<anyhow::Error>>> = Arc::new(Mutex::new(None));
    let worker = &worker;

    thread::scope(|scope| {
        for _ in 0..jobs {
            let queue = Arc::clone(&queue);
            let records = Arc::clone(&records);
            let first_error = Arc::clone(&first_error);
            let progress = progress.clone();

            scope.spawn(move || {
                loop {
                    if has_error(first_error.as_ref()) {
                        break;
                    }

                    let item = match queue.lock() {
                        Ok(mut guard) => guard.pop_front(),
                        Err(_) => {
                            store_error(
                                first_error.as_ref(),
                                anyhow!("{queue_label} lock poisoned"),
                            );
                            break;
                        }
                    };

                    let Some(item) = item else {
                        break;
                    };

                    match worker(item) {
                        Ok(record) => match records.lock() {
                            Ok(mut guard) => guard.push(record),
                            Err(_) => {
                                store_error(
                                    first_error.as_ref(),
                                    anyhow!("{records_label} lock poisoned"),
                                );
                                break;
                            }
                        },
                        Err(err) => {
                            store_error(first_error.as_ref(), err);
                            break;
                        }
                    }

                    if let Some(bar) = progress.as_ref() {
                        bar.inc(1);
                    }
                }
            });
        }
    });

    if let Some(bar) = progress {
        bar.finish_and_clear();
    }

    let mut error_guard = first_error
        .lock()
        .map_err(|_| anyhow!("error state lock poisoned"))?;
    if let Some(err) = error_guard.take() {
        return Err(err);
    }

    let mut records_guard = records
        .lock()
        .map_err(|_| anyhow!("{records_label} lock poisoned"))?;
    Ok(std::mem::take(&mut *records_guard))
}

fn build_progress(total: u64) -> Option<ProgressBar> {
    if !std::io::stdout().is_terminal() {
        return None;
    }

    let bar = ProgressBar::new(total);
    let style = ProgressStyle::with_template("[{elapsed_precise}] {bar:40.cyan/blue} {pos}/{len}")
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("#>-");
    bar.set_style(style);
    Some(bar)
}

fn has_error(first_error: &Mutex<Option<anyhow::Error>>) -> bool {
    match first_error.lock() {
        Ok(guard) => guard.is_some(),
        Err(_) => true,
    }
}

fn store_error(first_error: &Mutex<Option<anyhow::Error>>, err: anyhow::Error) {
    if let Ok(mut guard) = first_error.lock()
        && guard.is_none()
    {
        *guard = Some(err);
    }
}

fn resolve_manifest_path(
    manifest_path: Option<PathBuf>,
    project_path: Option<PathBuf>,
) -> Result<PathBuf> {
    let path = match (manifest_path, project_path) {
        (Some(path), None) => path,
        (None, Some(path)) => manifest_from_project_path(path),
        (None, None) => env::current_dir()?
            .ancestors()
            .map(|dir| dir.join("Cargo.toml"))
            .find(|path| path.is_file())
            .ok_or_else(|| {
                anyhow!(
                    "no Cargo.toml found in this directory or its parents; use --path <PROJECT>"
                )
            })?,
        (Some(_), Some(_)) => bail!("use either `--manifest-path` or `--path`, not both"),
    };
    canonical_manifest_path(path)
}

fn manifest_from_project_path(path: PathBuf) -> PathBuf {
    if path.is_dir() {
        path.join("Cargo.toml")
    } else {
        path
    }
}

fn canonical_manifest_path(path: PathBuf) -> Result<PathBuf> {
    let canonical = fs::canonicalize(&path)
        .with_context(|| format!("cannot resolve manifest path {}", path.display()))?;
    if !canonical.is_file() {
        bail!("manifest path is not a file: {}", canonical.display());
    }
    Ok(canonical)
}

fn read_manifest_metadata(manifest_path: &Path) -> Result<WorkspaceManifest> {
    let workspace_manifest_path = discover_workspace_manifest(manifest_path)?;
    let workspace_manifest = read_cargo_manifest(&workspace_manifest_path)?;
    let workspace_root = manifest_parent(&workspace_manifest_path)?.to_path_buf();
    let workspace_dependencies = workspace_manifest
        .workspace
        .as_ref()
        .map(|workspace| &workspace.dependencies);
    let workspace_rust_version = workspace_manifest
        .workspace
        .as_ref()
        .and_then(|workspace| workspace.package.rust_version.as_deref());

    let member_manifest_paths =
        workspace_member_manifest_paths(&workspace_root, &workspace_manifest)?;
    let mut packages = Vec::new();

    for path in member_manifest_paths {
        let manifest = read_cargo_manifest(&path)?;
        let Some(package) = manifest.package.as_ref() else {
            continue;
        };

        let rust_version = package
            .rust_version
            .as_ref()
            .and_then(|value| value.resolve(workspace_rust_version));
        let default_enabled_optional =
            default_enabled_optional_dependencies(&manifest, workspace_dependencies);
        packages.push(WorkspacePackage {
            rust_version,
            dependencies: collect_manifest_dependencies(
                &manifest,
                workspace_dependencies,
                &default_enabled_optional,
            ),
        });
    }

    Ok(WorkspaceManifest {
        packages,
        workspace_root,
    })
}

fn discover_workspace_manifest(manifest_path: &Path) -> Result<PathBuf> {
    let selected_manifest = fs::canonicalize(manifest_path)
        .with_context(|| format!("cannot resolve manifest path {}", manifest_path.display()))?;
    let selected = read_cargo_manifest(&selected_manifest)?;
    if let Some(workspace) = selected
        .package
        .as_ref()
        .and_then(|package| package.workspace.as_deref())
    {
        let workspace_path = manifest_parent(&selected_manifest)?.join(workspace);
        let workspace_manifest = if workspace_path.is_dir() {
            workspace_path.join("Cargo.toml")
        } else {
            workspace_path
        };
        let workspace_manifest = fs::canonicalize(&workspace_manifest).with_context(|| {
            format!(
                "cannot resolve explicit package workspace {}",
                workspace_manifest.display()
            )
        })?;
        let manifest = read_cargo_manifest(&workspace_manifest)?;
        if manifest.workspace.is_none() {
            bail!(
                "explicit package workspace has no `[workspace]` table: {}",
                workspace_manifest.display()
            );
        }
        return Ok(workspace_manifest);
    }
    let mut dir = Some(manifest_parent(&selected_manifest)?);

    while let Some(current_dir) = dir {
        let candidate = current_dir.join("Cargo.toml");
        if candidate.is_file() {
            let candidate = fs::canonicalize(&candidate)
                .with_context(|| format!("cannot resolve manifest path {}", candidate.display()))?;
            let manifest = read_cargo_manifest(&candidate)?;
            if manifest.workspace.is_some() {
                if candidate == selected_manifest {
                    return Ok(candidate);
                }
                let root = manifest_parent(&candidate)?;
                if workspace_includes_manifest(root, &manifest, &selected_manifest)? {
                    return Ok(candidate);
                }
            }
        }
        dir = current_dir.parent();
    }

    Ok(selected_manifest)
}

fn workspace_includes_manifest(
    workspace_root: &Path,
    manifest: &CargoManifest,
    selected_manifest: &Path,
) -> Result<bool> {
    Ok(workspace_member_manifest_paths(workspace_root, manifest)?
        .into_iter()
        .any(|path| path == selected_manifest))
}

fn workspace_member_manifest_paths(
    workspace_root: &Path,
    manifest: &CargoManifest,
) -> Result<Vec<PathBuf>> {
    let mut paths = BTreeSet::new();
    if manifest.package.is_some() {
        paths.insert(
            fs::canonicalize(workspace_root.join("Cargo.toml")).with_context(|| {
                format!(
                    "cannot resolve manifest path {}",
                    workspace_root.join("Cargo.toml").display()
                )
            })?,
        );
    }

    let Some(workspace) = manifest.workspace.as_ref() else {
        return Ok(paths.into_iter().collect());
    };

    let mut excluded = BTreeSet::new();
    for pattern in &workspace.exclude {
        for path in expand_workspace_member_pattern(workspace_root, pattern)? {
            excluded.insert(path);
        }
    }

    for pattern in &workspace.members {
        for path in expand_workspace_member_pattern(workspace_root, pattern)? {
            if !excluded.contains(&path) {
                paths.insert(path);
            }
        }
    }

    let mut queue = VecDeque::from_iter(paths.iter().cloned());
    let mut scanned = BTreeSet::new();
    while let Some(manifest_path) = queue.pop_front() {
        if !scanned.insert(manifest_path.clone()) {
            continue;
        }
        let member = read_cargo_manifest(&manifest_path)?;
        for dependency in manifest_path_dependencies(&member, Some(&workspace.dependencies)) {
            let dependency_root = if dependency.workspace_relative {
                workspace_root
            } else {
                manifest_parent(&manifest_path)?
            };
            let candidate = dependency_root.join(dependency.path);
            let candidate = if candidate.is_dir() {
                candidate.join("Cargo.toml")
            } else {
                candidate
            };
            if !candidate.is_file() {
                continue;
            }
            let candidate = fs::canonicalize(&candidate).with_context(|| {
                format!(
                    "cannot resolve path dependency manifest {}",
                    candidate.display()
                )
            })?;
            if candidate.starts_with(workspace_root)
                && !excluded.contains(&candidate)
                && paths.insert(candidate.clone())
            {
                queue.push_back(candidate);
            }
        }
    }

    Ok(paths.into_iter().collect())
}

fn manifest_path_dependencies(
    manifest: &CargoManifest,
    workspace_dependencies: Option<&BTreeMap<String, ManifestDependency>>,
) -> Vec<ManifestPathDependency> {
    manifest_dependency_aliases(manifest)
        .into_iter()
        .filter_map(|(alias, dependency)| {
            let attrs = ManifestDependencyAttrs::from_dependency(dependency);
            if attrs.workspace {
                let path = workspace_dependencies
                    .and_then(|dependencies| dependencies.get(alias))
                    .map(ManifestDependencyAttrs::from_dependency)?
                    .path?;
                Some(ManifestPathDependency {
                    path,
                    workspace_relative: true,
                })
            } else {
                attrs.path.map(|path| ManifestPathDependency {
                    path,
                    workspace_relative: false,
                })
            }
        })
        .collect()
}

fn expand_workspace_member_pattern(workspace_root: &Path, pattern: &str) -> Result<Vec<PathBuf>> {
    let parts = pattern
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let mut candidates = Vec::new();
    expand_workspace_pattern_parts(workspace_root, &parts, &mut candidates)?;

    let mut manifests = Vec::new();
    for candidate in candidates {
        let manifest = if candidate.is_dir() {
            candidate.join("Cargo.toml")
        } else {
            candidate
        };
        if manifest.is_file() {
            manifests.push(
                fs::canonicalize(&manifest).with_context(|| {
                    format!("cannot resolve manifest path {}", manifest.display())
                })?,
            );
        }
    }
    manifests.sort();
    manifests.dedup();
    Ok(manifests)
}

fn expand_workspace_pattern_parts(
    current: &Path,
    parts: &[&str],
    out: &mut Vec<PathBuf>,
) -> Result<()> {
    let Some((part, rest)) = parts.split_first() else {
        out.push(current.to_path_buf());
        return Ok(());
    };

    if *part == "**" {
        // Cargo workspace globs use `**` for zero or more path components.
        expand_workspace_pattern_parts(current, rest, out)?;
        if !current.is_dir() {
            return Ok(());
        }
        let mut entries = fs::read_dir(current)
            .with_context(|| format!("read workspace directory {}", current.display()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| format!("read workspace entries {}", current.display()))?;
        entries.sort_by_key(|entry| entry.path());
        for entry in entries {
            if entry
                .file_type()
                .with_context(|| format!("read file type for {}", entry.path().display()))?
                .is_dir()
            {
                expand_workspace_pattern_parts(&entry.path(), parts, out)?;
            }
        }
        return Ok(());
    }

    if part.contains(['*', '?']) {
        if !current.is_dir() {
            return Ok(());
        }
        let mut entries = fs::read_dir(current)
            .with_context(|| format!("read workspace directory {}", current.display()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| format!("read workspace entries {}", current.display()))?;
        entries.sort_by_key(|entry| entry.path());
        for entry in entries {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if wildcard_component_matches(part, name) {
                expand_workspace_pattern_parts(&entry.path(), rest, out)?;
            }
        }
        return Ok(());
    }

    expand_workspace_pattern_parts(&current.join(part), rest, out)
}

fn wildcard_component_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.chars().collect::<Vec<_>>();
    let value = value.chars().collect::<Vec<_>>();
    let mut previous = vec![false; value.len() + 1];
    previous[0] = true;
    for token in pattern {
        let mut current = vec![false; value.len() + 1];
        match token {
            '*' => {
                current[0] = previous[0];
                for index in 1..=value.len() {
                    current[index] = previous[index] || current[index - 1];
                }
            }
            '?' => {
                current[1..(value.len() + 1)].copy_from_slice(&previous[..value.len()]);
            }
            literal => {
                for index in 1..=value.len() {
                    current[index] = previous[index - 1] && value[index - 1] == literal;
                }
            }
        }
        previous = current;
    }
    previous[value.len()]
}

fn read_cargo_manifest(path: &Path) -> Result<CargoManifest> {
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    toml::from_str::<CargoManifest>(&raw)
        .with_context(|| format!("parse Cargo manifest {}", path.display()))
}

fn manifest_parent(manifest_path: &Path) -> Result<&Path> {
    manifest_path
        .parent()
        .ok_or_else(|| anyhow!("manifest path has no parent: {}", manifest_path.display()))
}

fn collect_manifest_dependencies(
    manifest: &CargoManifest,
    workspace_dependencies: Option<&BTreeMap<String, ManifestDependency>>,
    default_enabled_optional: &BTreeSet<String>,
) -> Vec<WorkspaceDependency> {
    let mut deps = Vec::new();
    collect_manifest_dependency_table(
        &mut deps,
        &manifest.dependencies,
        None,
        workspace_dependencies,
        default_enabled_optional,
    );
    collect_manifest_dependency_table(
        &mut deps,
        &manifest.dev_dependencies,
        Some("dev"),
        workspace_dependencies,
        default_enabled_optional,
    );
    collect_manifest_dependency_table(
        &mut deps,
        &manifest.build_dependencies,
        Some("build"),
        workspace_dependencies,
        default_enabled_optional,
    );

    for target in manifest.target.values() {
        collect_manifest_dependency_table(
            &mut deps,
            &target.dependencies,
            None,
            workspace_dependencies,
            default_enabled_optional,
        );
        collect_manifest_dependency_table(
            &mut deps,
            &target.dev_dependencies,
            Some("dev"),
            workspace_dependencies,
            default_enabled_optional,
        );
        collect_manifest_dependency_table(
            &mut deps,
            &target.build_dependencies,
            Some("build"),
            workspace_dependencies,
            default_enabled_optional,
        );
    }

    deps
}

fn collect_manifest_dependency_table(
    out: &mut Vec<WorkspaceDependency>,
    table: &BTreeMap<String, ManifestDependency>,
    kind: Option<&str>,
    workspace_dependencies: Option<&BTreeMap<String, ManifestDependency>>,
    default_enabled_optional: &BTreeSet<String>,
) {
    for (alias, dep) in table {
        if let Some(resolved) = resolve_manifest_dependency(alias, dep, workspace_dependencies) {
            out.push(WorkspaceDependency {
                name: resolved.name,
                source: resolved.source,
                req: resolved.req,
                kind: kind.map(ToOwned::to_owned),
                optional: resolved.optional,
                enabled_by_default: default_enabled_optional.contains(alias),
            });
        }
    }
}

fn resolve_manifest_dependency(
    alias: &str,
    dep: &ManifestDependency,
    workspace_dependencies: Option<&BTreeMap<String, ManifestDependency>>,
) -> Option<ResolvedManifestDependency> {
    let attrs = ManifestDependencyAttrs::from_dependency(dep);
    let attrs = if attrs.workspace {
        let workspace_attrs = workspace_dependencies
            .and_then(|deps| deps.get(alias))
            .map(ManifestDependencyAttrs::from_dependency)?;
        attrs.overlay_workspace(workspace_attrs)
    } else {
        attrs
    };

    let source = if let Some(path) = attrs.path {
        DependencySource::Path(path)
    } else if let Some(git) = attrs.git {
        DependencySource::Git(git)
    } else if let Some(registry) = attrs.registry.or(attrs.registry_index) {
        DependencySource::Registry(registry)
    } else {
        DependencySource::CratesIo
    };
    let requirement = attrs
        .version
        .or_else(|| attrs.rev.map(|value| format!("rev:{value}")))
        .or_else(|| attrs.tag.map(|value| format!("tag:{value}")))
        .or_else(|| attrs.branch.map(|value| format!("branch:{value}")))
        .unwrap_or_else(|| "*".to_string());

    Some(ResolvedManifestDependency {
        name: attrs.package.unwrap_or_else(|| alias.to_string()),
        req: requirement,
        source,
        optional: attrs.optional.unwrap_or(false),
    })
}

fn default_enabled_optional_dependencies(
    manifest: &CargoManifest,
    workspace_dependencies: Option<&BTreeMap<String, ManifestDependency>>,
) -> BTreeSet<String> {
    let optional_aliases = manifest_dependency_aliases(manifest)
        .into_iter()
        .filter(|(alias, dep)| {
            resolve_manifest_dependency(alias, dep, workspace_dependencies)
                .is_some_and(|resolved| resolved.optional)
        })
        .map(|(alias, _)| alias.to_string())
        .collect::<BTreeSet<_>>();

    let mut active = BTreeSet::new();
    let mut visited_features = BTreeSet::new();
    let mut queue = VecDeque::from(["default".to_string()]);
    while let Some(feature) = queue.pop_front() {
        if !visited_features.insert(feature.clone()) {
            continue;
        }
        let Some(entries) = manifest.features.get(&feature) else {
            if optional_aliases.contains(&feature) {
                active.insert(feature);
            }
            continue;
        };
        for entry in entries {
            if let Some(dep) = entry.strip_prefix("dep:") {
                if optional_aliases.contains(dep) {
                    active.insert(dep.to_string());
                }
                continue;
            }
            if let Some((dep, _)) = entry.split_once('/') {
                if !dep.ends_with('?') && optional_aliases.contains(dep) {
                    active.insert(dep.to_string());
                }
                continue;
            }
            if manifest.features.contains_key(entry) {
                queue.push_back(entry.clone());
            } else if optional_aliases.contains(entry) {
                active.insert(entry.clone());
            }
        }
    }
    active
}

fn manifest_dependency_aliases(manifest: &CargoManifest) -> Vec<(&str, &ManifestDependency)> {
    let mut dependencies = Vec::new();
    dependencies.extend(
        manifest
            .dependencies
            .iter()
            .map(|(name, dep)| (name.as_str(), dep)),
    );
    dependencies.extend(
        manifest
            .dev_dependencies
            .iter()
            .map(|(name, dep)| (name.as_str(), dep)),
    );
    dependencies.extend(
        manifest
            .build_dependencies
            .iter()
            .map(|(name, dep)| (name.as_str(), dep)),
    );
    for target in manifest.target.values() {
        dependencies.extend(
            target
                .dependencies
                .iter()
                .map(|(name, dep)| (name.as_str(), dep)),
        );
        dependencies.extend(
            target
                .dev_dependencies
                .iter()
                .map(|(name, dep)| (name.as_str(), dep)),
        );
        dependencies.extend(
            target
                .build_dependencies
                .iter()
                .map(|(name, dep)| (name.as_str(), dep)),
        );
    }
    dependencies
}

fn collect_dependency_specs(
    metadata: &WorkspaceManifest,
    include_dev: bool,
    include_build: bool,
    include_optional: bool,
) -> Result<Vec<DependencySpec>> {
    Ok(collect_dependency_inventory(metadata, include_dev, include_build, include_optional)?.specs)
}

fn collect_dependency_inventory(
    metadata: &WorkspaceManifest,
    include_dev: bool,
    include_build: bool,
    include_optional: bool,
) -> Result<DependencyInventory> {
    let mut collector = DependencyCollector::new(metadata, include_dev, include_build);
    collector.collect_declared_dependency_specs(include_optional);
    Ok(collector.finish())
}

struct DependencyCollector<'a> {
    metadata: &'a WorkspaceManifest,
    include_dev: bool,
    include_build: bool,
    collected: BTreeMap<DependencyKey, DependencySpecBuilder>,
    skipped_local: BTreeMap<DependencyKey, DependencySpecBuilder>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct DependencyKey {
    name: String,
    source: DependencySource,
}

struct CollectedDependencyEntry {
    name: String,
    source: DependencySource,
    requirement: String,
    kind: String,
    project_rust_version: Option<String>,
    optional: bool,
}

impl<'a> DependencyCollector<'a> {
    fn new(metadata: &'a WorkspaceManifest, include_dev: bool, include_build: bool) -> Self {
        Self {
            metadata,
            include_dev,
            include_build,
            collected: BTreeMap::new(),
            skipped_local: BTreeMap::new(),
        }
    }

    fn collect_declared_dependency_specs(&mut self, include_optional: bool) {
        for package in &self.metadata.packages {
            let entries = package
                .dependencies
                .iter()
                .filter_map(|dep| self.declared_dependency_entry(package, dep, include_optional))
                .collect::<Vec<_>>();

            for entry in entries {
                self.insert_entry(entry);
            }
        }
    }

    fn declared_dependency_entry(
        &self,
        package: &WorkspacePackage,
        dep: &WorkspaceDependency,
        include_optional: bool,
    ) -> Option<CollectedDependencyEntry> {
        if dep.optional && !dep.enabled_by_default && !include_optional {
            return None;
        }

        let kind = dependency_kind(dep.kind.as_deref());
        if !should_include_kind(kind, self.include_dev, self.include_build) {
            return None;
        }

        Some(CollectedDependencyEntry {
            name: dep.name.clone(),
            source: dep.source.clone(),
            requirement: dep.req.clone(),
            kind: kind.to_string(),
            project_rust_version: package.rust_version.clone(),
            optional: dep.optional,
        })
    }

    fn insert_entry(&mut self, entry: CollectedDependencyEntry) {
        Self::insert_entry_into(&mut self.collected, &mut self.skipped_local, entry);
    }

    fn insert_entry_into(
        collected: &mut BTreeMap<DependencyKey, DependencySpecBuilder>,
        skipped_local: &mut BTreeMap<DependencyKey, DependencySpecBuilder>,
        entry: CollectedDependencyEntry,
    ) {
        let target = if entry.source.is_local() {
            skipped_local
        } else {
            collected
        };
        insert_dependency_spec(
            target,
            entry.name,
            entry.source,
            entry.requirement,
            entry.kind,
            entry.project_rust_version,
            entry.optional,
        );
    }

    fn finish(self) -> DependencyInventory {
        DependencyInventory {
            specs: build_dependency_specs(self.collected),
            skipped_local: build_dependency_specs(self.skipped_local),
        }
    }
}

fn build_dependency_specs(
    collected: BTreeMap<DependencyKey, DependencySpecBuilder>,
) -> Vec<DependencySpec> {
    let mut out = Vec::with_capacity(collected.len());
    for (key, builder) in collected {
        out.push(DependencySpec {
            name: key.name,
            source: key.source,
            requirement: join_requirements(&builder.requirements),
            kinds: join_set(&builder.kinds),
            project_rust_version: builder
                .project_rust_versions_complete
                .then(|| minimum_rust_version(&builder.project_rust_versions))
                .flatten(),
            optional: builder.optional,
        });
    }
    out
}

fn insert_dependency_spec(
    collected: &mut BTreeMap<DependencyKey, DependencySpecBuilder>,
    name: String,
    source: DependencySource,
    requirement: String,
    kind: String,
    project_rust_version: Option<String>,
    optional: bool,
) {
    let entry = collected.entry(DependencyKey { name, source }).or_default();
    entry.requirements.insert(requirement);
    entry.kinds.insert(kind);
    if let Some(rust_version) = project_rust_version {
        entry.project_rust_versions.insert(rust_version);
    } else {
        entry.project_rust_versions_complete = false;
    }
    entry.optional = entry.optional && optional;
}

fn dependency_kind(kind: Option<&str>) -> &str {
    kind.unwrap_or("normal")
}

fn should_include_kind(kind: &str, include_dev: bool, include_build: bool) -> bool {
    match kind {
        "normal" => true,
        "dev" => include_dev,
        "build" => include_build,
        _ => false,
    }
}

fn normalize_dependency_name(name: &str) -> String {
    name.replace('-', "_")
}

fn dependency_label(count: usize) -> &'static str {
    if count == 1 {
        "dependency"
    } else {
        "dependencies"
    }
}

fn render_action_audit_count(count: usize) -> String {
    if count == 0 {
        String::new()
    } else {
        format!(" and {count} workflow {}", action_label(count))
    }
}

fn action_label(count: usize) -> &'static str {
    if count == 1 { "action" } else { "actions" }
}

fn join_set(set: &BTreeSet<String>) -> String {
    set.iter().cloned().collect::<Vec<_>>().join(",")
}

fn join_requirements(requirements: &BTreeSet<String>) -> String {
    requirements.iter().cloned().collect::<Vec<_>>().join(" | ")
}

#[derive(Debug, Clone)]
struct WorkflowActionSpec {
    action: String,
    owner: String,
    repo: String,
    path: Option<String>,
    ref_name: String,
    version_hint: Option<String>,
    locations: Vec<ActionLocation>,
}

impl WorkflowActionSpec {
    fn key(&self) -> String {
        format!("{}@{}", self.action, self.ref_name)
    }
}

fn collect_workflow_action_specs(project_root: &Path) -> Result<Vec<WorkflowActionSpec>> {
    let workflows_dir = project_root.join(".github").join("workflows");
    if !workflows_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut grouped = BTreeMap::<String, WorkflowActionSpec>::new();
    let mut files = fs::read_dir(&workflows_dir)
        .with_context(|| format!("read workflow directory {}", workflows_dir.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("read workflow entries {}", workflows_dir.display()))?;
    files.sort_by_key(|entry| entry.path());

    for entry in files {
        let path = entry.path();
        if !is_workflow_yaml(&path) {
            continue;
        }
        let content =
            fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let display_path = path
            .strip_prefix(project_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        for (line_index, line) in content.lines().enumerate() {
            let Some(mut spec) = parse_workflow_uses_line(line) else {
                continue;
            };
            spec.locations.push(ActionLocation {
                file: display_path.clone(),
                line: line_index + 1,
            });
            let key = spec.key();
            if let Some(existing) = grouped.get_mut(&key) {
                existing.locations.extend(spec.locations);
                if existing.version_hint.is_none() {
                    existing.version_hint = spec.version_hint;
                }
            } else {
                grouped.insert(key, spec);
            }
        }
    }

    Ok(grouped.into_values().collect())
}

fn is_workflow_yaml(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| matches!(ext, "yml" | "yaml"))
}

fn parse_workflow_uses_line(line: &str) -> Option<WorkflowActionSpec> {
    let trimmed = line.trim_start();
    let trimmed = trimmed.strip_prefix("- ").unwrap_or(trimmed).trim_start();
    let raw_value = trimmed.strip_prefix("uses:")?.trim();
    let (value, version_hint) = normalize_workflow_uses_value(raw_value)?;
    if value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with("docker://")
        || value.starts_with("${{")
    {
        return None;
    }

    let spec = resolve::ActionSpec::parse(&value).ok()?;
    Some(WorkflowActionSpec {
        action: spec.action_without_ref(),
        owner: spec.owner,
        repo: spec.repo,
        path: spec.path,
        ref_name: spec.ref_name,
        version_hint,
        locations: Vec::new(),
    })
}

fn normalize_workflow_uses_value(raw: &str) -> Option<(String, Option<String>)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (without_comment, comment) = trimmed
        .split_once(" #")
        .map_or((trimmed, None), |(value, comment)| (value, Some(comment)));
    let without_comment = without_comment.trim();
    let value = strip_matching_quotes(without_comment).unwrap_or(without_comment);
    let version_hint = comment.and_then(extract_action_version_hint);
    (!value.is_empty()).then(|| (value.to_string(), version_hint))
}

fn extract_action_version_hint(comment: &str) -> Option<String> {
    comment
        .split(|character: char| {
            character.is_ascii_whitespace() || character == ',' || character == ';'
        })
        .map(|token| {
            token.trim_matches(|character: char| matches!(character, '(' | ')' | '[' | ']'))
        })
        .find(|token| parse_action_tag_version(token).is_some())
        .map(ToOwned::to_owned)
}

fn strip_matching_quotes(value: &str) -> Option<&str> {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\'')
            || (bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"'))
    {
        return Some(&value[1..value.len() - 1]);
    }
    None
}

fn valid_action_segment(segment: &str) -> Option<&str> {
    let segment = segment.trim();
    if segment.is_empty()
        || segment
            .bytes()
            .any(|b| !(b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')))
    {
        return None;
    }
    Some(segment)
}

fn sort_records(records: &mut [DepAuditRecord]) {
    records.sort_by(|a, b| {
        b.risk
            .weight()
            .cmp(&a.risk.weight())
            .then_with(|| update_plan_weight(b).cmp(&update_plan_weight(a)))
            .then_with(|| a.name.cmp(&b.name))
    });
}

fn update_plan_weight(record: &DepAuditRecord) -> u8 {
    record.update_plan.map_or(0, DependencyUpdatePlan::weight)
}

fn sort_action_records(records: &mut [ActionAuditRecord]) {
    records.sort_by(|a, b| {
        b.update_plan
            .weight()
            .cmp(&a.update_plan.weight())
            .then_with(|| a.action.cmp(&b.action))
            .then_with(|| a.current_ref.cmp(&b.current_ref))
    });
}

fn build_action_audit_record(
    spec: WorkflowActionSpec,
    latest_tags: std::result::Result<Vec<GitHubTagSnapshot>, String>,
) -> ActionAuditRecord {
    let mut record = ActionAuditRecord {
        action: spec.action,
        owner: spec.owner,
        repo: spec.repo,
        path: spec.path,
        current_ref: spec.ref_name,
        latest_ref: None,
        latest_sha: None,
        update_plan: ActionUpdatePlan::Review,
        note: None,
        locations: spec.locations,
    };

    let tags = match latest_tags {
        Ok(tags) => tags,
        Err(err) => {
            record.note = Some(format!("GitHub query failed: {err}"));
            return record;
        }
    };
    let Some(latest) = latest_stable_action_tag(&tags) else {
        record.note = Some("no semver tags found".to_string());
        return record;
    };

    record.latest_ref = Some(latest.tag.clone());
    record.latest_sha = latest.sha.clone();
    let sha_pinned = is_full_commit_sha(&record.current_ref);
    let tagged_current = if sha_pinned {
        tags.iter()
            .filter(|tag| tag.sha.eq_ignore_ascii_case(&record.current_ref))
            .filter_map(action_tag_version)
            .max_by(compare_action_tag_versions)
    } else {
        None
    };
    let current_from_hint = sha_pinned && tagged_current.is_none();
    let current = if tagged_current.is_some() {
        tagged_current
    } else if sha_pinned {
        spec.version_hint
            .as_deref()
            .and_then(parse_action_tag_version)
    } else {
        parse_action_tag_version(&record.current_ref)
    };
    let Some(current) = current else {
        record.note = Some(if is_full_commit_sha(&record.current_ref) {
            "sha pin has no semver tag; review manually".to_string()
        } else {
            "floating or non-semver ref; review manually".to_string()
        });
        return record;
    };
    if current_from_hint {
        if current.precision != ActionTagPrecision::Patch {
            record.note = Some("sha pin needs an exact version hint; review manually".to_string());
            return record;
        }
        if current.version == latest.version
            && latest
                .sha
                .as_deref()
                .is_some_and(|sha| !sha.eq_ignore_ascii_case(&record.current_ref))
        {
            record.note = Some("sha pin does not match the hinted release tag".to_string());
            return record;
        }
    }

    if current_action_ref_is_outdated(&current, &latest) {
        record.update_plan = ActionUpdatePlan::Bump;
        record.note = Some("newer action tag available".to_string());
    } else {
        record.update_plan = ActionUpdatePlan::Keep;
        record.note = Some(if sha_pinned {
            "sha-pinned and up to date".to_string()
        } else {
            "current ref is up to date".to_string()
        });
    }
    record
}

fn compare_action_tag_versions(
    left: &ActionTagVersion,
    right: &ActionTagVersion,
) -> std::cmp::Ordering {
    left.version
        .cmp(&right.version)
        .then_with(|| left.precision.weight().cmp(&right.precision.weight()))
        .then_with(|| left.tag.cmp(&right.tag))
}

fn action_tag_version(tag: &GitHubTagSnapshot) -> Option<ActionTagVersion> {
    let mut version = parse_action_tag_version(&tag.name)?;
    version.sha = Some(tag.sha.clone());
    Some(version)
}

fn latest_stable_action_tag(tags: &[GitHubTagSnapshot]) -> Option<ActionTagVersion> {
    tags.iter()
        .filter_map(action_tag_version)
        .filter(|version| version.version.pre.is_empty())
        .max_by(compare_action_tag_versions)
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct ActionTagVersion {
    tag: String,
    version: Version,
    precision: ActionTagPrecision,
    sha: Option<String>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ActionTagPrecision {
    Major,
    Minor,
    Patch,
}

impl ActionTagPrecision {
    fn weight(self) -> u8 {
        match self {
            Self::Major => 1,
            Self::Minor => 2,
            Self::Patch => 3,
        }
    }
}

fn parse_action_tag_version(tag: &str) -> Option<ActionTagVersion> {
    let raw = tag.trim();
    let version = raw
        .strip_prefix('v')
        .or_else(|| raw.strip_prefix('V'))
        .unwrap_or(raw);
    let parts = version.split('.').collect::<Vec<_>>();
    if !(1..=3).contains(&parts.len()) || parts.iter().any(|part| part.is_empty()) {
        return None;
    }
    if parts
        .iter()
        .any(|part| part.bytes().any(|byte| !byte.is_ascii_digit()))
    {
        return None;
    }

    let normalized = match parts.len() {
        1 => format!("{}.0.0", parts[0]),
        2 => format!("{}.{}.0", parts[0], parts[1]),
        3 => version.to_string(),
        _ => return None,
    };
    let parsed = Version::parse(&normalized).ok()?;
    Some(ActionTagVersion {
        tag: raw.to_string(),
        version: parsed,
        precision: match parts.len() {
            1 => ActionTagPrecision::Major,
            2 => ActionTagPrecision::Minor,
            3 => ActionTagPrecision::Patch,
            _ => return None,
        },
        sha: None,
    })
}

fn current_action_ref_is_outdated(current: &ActionTagVersion, latest: &ActionTagVersion) -> bool {
    match current.precision {
        ActionTagPrecision::Major => latest.version.major > current.version.major,
        ActionTagPrecision::Minor => {
            (latest.version.major, latest.version.minor)
                > (current.version.major, current.version.minor)
        }
        ActionTagPrecision::Patch => latest.version > current.version,
    }
}

fn is_full_commit_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

// Keep tag and immutable commit together so update advice can preserve SHA pinning.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
struct GitHubTagSnapshot {
    name: String,
    sha: String,
}

#[derive(Debug, Clone, Deserialize)]
struct GitHubTagResponse {
    name: String,
    commit: GitHubTagCommitResponse,
}

#[derive(Debug, Clone, Deserialize)]
struct GitHubTagCommitResponse {
    sha: String,
}

#[derive(Debug, Deserialize, Default)]
struct CargoManifest {
    #[serde(default)]
    package: Option<ManifestPackage>,
    #[serde(default)]
    workspace: Option<ManifestWorkspace>,
    #[serde(default, rename = "dependencies")]
    dependencies: BTreeMap<String, ManifestDependency>,
    #[serde(default, rename = "dev-dependencies")]
    dev_dependencies: BTreeMap<String, ManifestDependency>,
    #[serde(default, rename = "build-dependencies")]
    build_dependencies: BTreeMap<String, ManifestDependency>,
    #[serde(default)]
    target: BTreeMap<String, ManifestTargetDependencies>,
    #[serde(default)]
    features: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct ManifestPackage {
    #[serde(default, rename = "rust-version")]
    rust_version: Option<InheritableString>,
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct ManifestWorkspace {
    #[serde(default)]
    members: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default, rename = "dependencies")]
    dependencies: BTreeMap<String, ManifestDependency>,
    #[serde(default)]
    package: ManifestWorkspacePackage,
}

#[derive(Debug, Deserialize, Default)]
struct ManifestWorkspacePackage {
    #[serde(default, rename = "rust-version")]
    rust_version: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct ManifestTargetDependencies {
    #[serde(default, rename = "dependencies")]
    dependencies: BTreeMap<String, ManifestDependency>,
    #[serde(default, rename = "dev-dependencies")]
    dev_dependencies: BTreeMap<String, ManifestDependency>,
    #[serde(default, rename = "build-dependencies")]
    build_dependencies: BTreeMap<String, ManifestDependency>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ManifestDependency {
    Simple(String),
    Detailed(ManifestDependencyDetail),
}

#[derive(Debug, Deserialize, Default)]
struct ManifestDependencyDetail {
    #[serde(default)]
    package: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    git: Option<String>,
    #[serde(default)]
    registry: Option<String>,
    #[serde(default, rename = "registry-index")]
    registry_index: Option<String>,
    #[serde(default)]
    rev: Option<String>,
    #[serde(default)]
    tag: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    workspace: bool,
    #[serde(default)]
    optional: Option<bool>,
}

#[derive(Debug, Default)]
struct ManifestDependencyAttrs {
    package: Option<String>,
    version: Option<String>,
    path: Option<String>,
    git: Option<String>,
    registry: Option<String>,
    registry_index: Option<String>,
    rev: Option<String>,
    tag: Option<String>,
    branch: Option<String>,
    workspace: bool,
    optional: Option<bool>,
}

impl ManifestDependencyAttrs {
    fn from_dependency(dep: &ManifestDependency) -> Self {
        match dep {
            ManifestDependency::Simple(version) => Self {
                version: Some(version.clone()),
                ..Self::default()
            },
            ManifestDependency::Detailed(detail) => Self {
                package: detail.package.clone(),
                version: detail.version.clone(),
                path: detail.path.clone(),
                git: detail.git.clone(),
                registry: detail.registry.clone(),
                registry_index: detail.registry_index.clone(),
                rev: detail.rev.clone(),
                tag: detail.tag.clone(),
                branch: detail.branch.clone(),
                workspace: detail.workspace,
                optional: detail.optional,
            },
        }
    }

    fn overlay_workspace(self, workspace: Self) -> Self {
        Self {
            package: self.package.or(workspace.package),
            version: self.version.or(workspace.version),
            path: self.path.or(workspace.path),
            git: self.git.or(workspace.git),
            registry: self.registry.or(workspace.registry),
            registry_index: self.registry_index.or(workspace.registry_index),
            rev: self.rev.or(workspace.rev),
            tag: self.tag.or(workspace.tag),
            branch: self.branch.or(workspace.branch),
            workspace: false,
            optional: self.optional.or(workspace.optional),
        }
    }
}

struct ResolvedManifestDependency {
    name: String,
    req: String,
    source: DependencySource,
    optional: bool,
}

struct ManifestPathDependency {
    path: String,
    workspace_relative: bool,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum InheritableString {
    Value(String),
    Workspace { workspace: bool },
}

impl InheritableString {
    fn resolve(&self, workspace_value: Option<&str>) -> Option<String> {
        match self {
            Self::Value(value) => Some(value.clone()),
            Self::Workspace { workspace: true } => workspace_value.map(ToOwned::to_owned),
            Self::Workspace { workspace: false } => None,
        }
    }
}

#[derive(Debug)]
struct WorkspaceManifest {
    packages: Vec<WorkspacePackage>,
    workspace_root: PathBuf,
}

#[derive(Debug)]
struct WorkspacePackage {
    rust_version: Option<String>,
    dependencies: Vec<WorkspaceDependency>,
}

#[derive(Debug)]
struct WorkspaceDependency {
    name: String,
    source: DependencySource,
    req: String,
    kind: Option<String>,
    optional: bool,
    enabled_by_default: bool,
}

#[derive(Debug, Default)]
struct DependencyInventory {
    specs: Vec<DependencySpec>,
    skipped_local: Vec<DependencySpec>,
}

impl DependencyInventory {
    fn skipped_local_count(&self) -> usize {
        self.skipped_local.len()
    }
}

#[derive(Debug, Deserialize)]
struct CratesApiResponse {
    #[serde(rename = "crate")]
    krate: CratesCrate,
    versions: Vec<CratesVersion>,
}

#[derive(Debug, Deserialize)]
struct CratesCrate {
    updated_at: Option<String>,
    max_stable_version: Option<String>,
    max_version: Option<String>,
    repository: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CratesVersion {
    num: String,
    created_at: String,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    rust_version: Option<String>,
    #[serde(default)]
    yanked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CrateSnapshot {
    max_version: String,
    updated_at: Option<String>,
    latest_release_at: Option<String>,
    repository: Option<String>,
    latest_version_license: Option<String>,
    latest_version_rust_version: Option<String>,
    latest_version_yanked: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GitHubRepoResponse {
    stargazers_count: u64,
    archived: bool,
    pushed_at: Option<String>,
}

#[cfg(test)]
mod tests;
