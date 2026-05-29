//! Dependency governance, version drift, and maintenance audit for Rust projects.

mod api;
#[path = "deps/latest.rs"]
mod latest;
mod model;
#[path = "deps/render.rs"]
mod render;

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
    DependencySpec, DependencySpecBuilder, DependencyUpdatePlan, GitHubCacheEntry, RiskLevel,
    age_days_from_now, classify_risk, github_repo_from_url, std_alternative,
};
use self::render::{build_summary, print_report, write_json_report};

const HTTP_TIMEOUT_SECS: u64 = 30;
const HTTP_USER_AGENT: &str = "za-deps-audit/0.1";
const HTTP_MAX_ATTEMPTS: usize = 3;
const HTTP_BACKOFF_BASE_MS: u64 = 200;
const AUTO_DEPS_JOBS_MULTIPLIER: usize = 2;
const AUTO_DEPS_JOBS_MIN: usize = 4;
const AUTO_DEPS_JOBS_MAX: usize = 16;
const DEPS_CACHE_SCHEMA_VERSION: u32 = 2;
const DEPS_CACHE_FILE_NAME: &str = "deps-cache-v2.json";
const CRATES_CACHE_TTL_SECS: u64 = 6 * 60 * 60;
const GITHUB_CACHE_TTL_SECS: u64 = 60 * 60;
const WORKFLOW_ACTION_REF_MAX_TAGS: usize = 100;

pub struct DepsRunOptions {
    pub manifest_path: Option<PathBuf>,
    pub project_path: Option<PathBuf>,
    pub github_token_override: Option<String>,
    pub jobs: Option<usize>,
    pub include_dev: bool,
    pub include_build: bool,
    pub include_optional: bool,
    pub json_out: Option<PathBuf>,
    pub fail_on_high: bool,
    pub verbose: bool,
}

pub struct DepsLatestOptions {
    pub crates: Vec<String>,
    pub manifest_path: Option<PathBuf>,
    pub project_path: Option<PathBuf>,
    pub jobs: Option<usize>,
    pub include_dev: bool,
    pub include_build: bool,
    pub include_optional: bool,
    pub json: bool,
    pub toml: bool,
    pub suggest: bool,
}

pub fn run(opts: DepsRunOptions) -> Result<()> {
    let DepsRunOptions {
        manifest_path,
        project_path,
        github_token_override,
        jobs,
        include_dev,
        include_build,
        include_optional,
        json_out,
        fail_on_high,
        verbose,
    } = opts;

    let manifest_path = resolve_manifest_path(manifest_path, project_path)?;
    let metadata = read_manifest_metadata(&manifest_path)?;
    let project_root = project_root_from_metadata(&metadata, &manifest_path)?;
    let inventory =
        collect_dependency_inventory(&metadata, include_dev, include_build, include_optional)?;
    let action_specs = collect_workflow_action_specs(&project_root)?;
    if inventory.specs.is_empty() && action_specs.is_empty() {
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
    let api = Arc::new(ApiClient::new(github_token_override)?);
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
        (None, None) => PathBuf::from("Cargo.toml"),
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

fn project_root_from_metadata(metadata: &CargoMetadata, manifest_path: &Path) -> Result<PathBuf> {
    if let Some(root) = metadata.workspace_root.as_ref()
        && !root.as_os_str().is_empty()
    {
        return fs::canonicalize(root)
            .with_context(|| format!("cannot resolve workspace root {}", root.display()));
    }
    manifest_path
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow!("manifest path has no parent: {}", manifest_path.display()))
}

fn read_manifest_metadata(manifest_path: &Path) -> Result<CargoMetadata> {
    let workspace_manifest_path = discover_workspace_manifest(manifest_path)?;
    let workspace_manifest = read_cargo_manifest(&workspace_manifest_path)?;
    let workspace_root = manifest_parent(&workspace_manifest_path)?.to_path_buf();
    let workspace_dependencies = workspace_manifest
        .workspace
        .as_ref()
        .map(|workspace| &workspace.dependencies);

    let member_manifest_paths =
        workspace_member_manifest_paths(&workspace_root, &workspace_manifest)?;
    let selected_manifest_path = fs::canonicalize(manifest_path)
        .with_context(|| format!("cannot resolve manifest path {}", manifest_path.display()))?;

    let mut packages = Vec::new();
    let mut workspace_members = Vec::new();
    let mut root = None;

    for path in member_manifest_paths {
        let manifest = read_cargo_manifest(&path)?;
        let Some(package) = manifest.package.as_ref() else {
            continue;
        };

        let id = manifest_package_id(&path);
        if path == selected_manifest_path {
            root = Some(id.clone());
        }
        workspace_members.push(id.clone());
        packages.push(CargoPackage {
            id,
            name: package.name.clone(),
            source: None,
            dependencies: collect_manifest_dependencies(&manifest, workspace_dependencies),
        });
    }

    if root.is_none() && workspace_members.len() == 1 {
        root = workspace_members.first().cloned();
    }

    Ok(CargoMetadata {
        packages,
        workspace_members,
        root,
        workspace_root: Some(workspace_root),
        resolve: None,
    })
}

fn discover_workspace_manifest(manifest_path: &Path) -> Result<PathBuf> {
    let selected_manifest = fs::canonicalize(manifest_path)
        .with_context(|| format!("cannot resolve manifest path {}", manifest_path.display()))?;
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

    Ok(paths.into_iter().collect())
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

    if part.contains('*') {
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
    if pattern == "*" {
        return true;
    }

    let mut remainder = value;
    let mut first = true;
    for segment in pattern.split('*') {
        if segment.is_empty() {
            continue;
        }
        if first && !pattern.starts_with('*') {
            let Some(stripped) = remainder.strip_prefix(segment) else {
                return false;
            };
            remainder = stripped;
        } else {
            let Some(index) = remainder.find(segment) else {
                return false;
            };
            remainder = &remainder[index + segment.len()..];
        }
        first = false;
    }

    pattern.ends_with('*') || remainder.is_empty()
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

fn manifest_package_id(manifest_path: &Path) -> String {
    manifest_path.display().to_string()
}

fn collect_manifest_dependencies(
    manifest: &CargoManifest,
    workspace_dependencies: Option<&BTreeMap<String, ManifestDependency>>,
) -> Vec<CargoDependency> {
    let mut deps = Vec::new();
    collect_manifest_dependency_table(
        &mut deps,
        &manifest.dependencies,
        None,
        workspace_dependencies,
    );
    collect_manifest_dependency_table(
        &mut deps,
        &manifest.dev_dependencies,
        Some("dev"),
        workspace_dependencies,
    );
    collect_manifest_dependency_table(
        &mut deps,
        &manifest.build_dependencies,
        Some("build"),
        workspace_dependencies,
    );

    for target in manifest.target.values() {
        collect_manifest_dependency_table(
            &mut deps,
            &target.dependencies,
            None,
            workspace_dependencies,
        );
        collect_manifest_dependency_table(
            &mut deps,
            &target.dev_dependencies,
            Some("dev"),
            workspace_dependencies,
        );
        collect_manifest_dependency_table(
            &mut deps,
            &target.build_dependencies,
            Some("build"),
            workspace_dependencies,
        );
    }

    deps
}

fn collect_manifest_dependency_table(
    out: &mut Vec<CargoDependency>,
    table: &BTreeMap<String, ManifestDependency>,
    kind: Option<&str>,
    workspace_dependencies: Option<&BTreeMap<String, ManifestDependency>>,
) {
    for (alias, dep) in table {
        if let Some(resolved) = resolve_manifest_dependency(alias, dep, workspace_dependencies) {
            out.push(CargoDependency {
                name: resolved.name,
                source: resolved
                    .crates_io
                    .then(|| "registry+https://github.com/rust-lang/crates.io-index".to_string()),
                req: resolved.req,
                kind: kind.map(ToOwned::to_owned),
                optional: resolved.optional,
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

    Some(ResolvedManifestDependency {
        name: attrs.package.unwrap_or_else(|| alias.to_string()),
        req: attrs.version.unwrap_or_else(|| "*".to_string()),
        crates_io: attrs.path.is_none() && attrs.git.is_none(),
        optional: attrs.optional.unwrap_or(false),
    })
}

fn collect_dependency_specs(
    metadata: &CargoMetadata,
    include_dev: bool,
    include_build: bool,
    include_optional: bool,
) -> Result<Vec<DependencySpec>> {
    Ok(collect_dependency_inventory(metadata, include_dev, include_build, include_optional)?.specs)
}

fn collect_dependency_inventory(
    metadata: &CargoMetadata,
    include_dev: bool,
    include_build: bool,
    include_optional: bool,
) -> Result<DependencyInventory> {
    let mut collector = DependencyCollector::new(metadata, include_dev, include_build);
    let used_resolve = collector.collect_resolved_dependency_specs()?;

    if include_optional {
        collector
            .collect_declared_dependency_specs(true, DeclaredDependencySelection::OptionalOnly)?;
    }

    if !used_resolve {
        collector.collect_declared_dependency_specs(
            include_optional,
            DeclaredDependencySelection::All,
        )?;
    }

    Ok(collector.finish())
}

struct DependencyCollector<'a> {
    metadata: &'a CargoMetadata,
    package_by_id: BTreeMap<&'a str, &'a CargoPackage>,
    package_ids: Vec<&'a str>,
    workspace_member_ids: BTreeSet<&'a str>,
    include_dev: bool,
    include_build: bool,
    collected: BTreeMap<String, DependencySpecBuilder>,
    skipped_local: BTreeMap<String, DependencySpecBuilder>,
}

struct CollectedDependencyEntry {
    local: bool,
    name: String,
    requirement: String,
    kind: String,
    optional: bool,
}

impl<'a> DependencyCollector<'a> {
    fn new(metadata: &'a CargoMetadata, include_dev: bool, include_build: bool) -> Self {
        let mut package_by_id: BTreeMap<&str, &CargoPackage> = BTreeMap::new();
        for pkg in &metadata.packages {
            package_by_id.insert(pkg.id.as_str(), pkg);
        }
        Self {
            metadata,
            package_by_id,
            package_ids: target_package_ids(metadata),
            workspace_member_ids: metadata
                .workspace_members
                .iter()
                .map(String::as_str)
                .collect(),
            include_dev,
            include_build,
            collected: BTreeMap::new(),
            skipped_local: BTreeMap::new(),
        }
    }

    fn collect_resolved_dependency_specs(&mut self) -> Result<bool> {
        let Some(resolve) = self.metadata.resolve.as_ref() else {
            return Ok(false);
        };

        let mut node_by_id: BTreeMap<&str, &CargoResolveNode> = BTreeMap::new();
        for node in &resolve.nodes {
            node_by_id.insert(node.id.as_str(), node);
        }

        let mut collected = BTreeMap::new();
        let mut skipped_local = BTreeMap::new();

        for package_id in self.package_ids.clone() {
            let package = self.package(package_id)?;
            let Some(node) = node_by_id.get(package_id) else {
                return Ok(false);
            };

            for dep in &node.deps {
                let dep_package = self
                    .package_by_id
                    .get(dep.pkg.as_str())
                    .ok_or_else(|| anyhow!("resolved dependency package not found: {}", dep.pkg))?;
                for entry in self.resolved_dependency_entries(package, dep_package, dep) {
                    Self::insert_entry_into(&mut collected, &mut skipped_local, entry);
                }
            }
        }

        self.collected = collected;
        self.skipped_local = skipped_local;
        Ok(true)
    }

    fn resolved_dependency_entries(
        &self,
        package: &CargoPackage,
        dep_package: &CargoPackage,
        dep: &CargoResolveNodeDep,
    ) -> Vec<CollectedDependencyEntry> {
        let mut entries = Vec::new();
        for kind in dependency_kinds_from_resolve(dep) {
            if !should_include_kind(kind, self.include_dev, self.include_build) {
                continue;
            }

            let declarations = matching_dependency_declarations(package, &dep_package.name, kind);
            let requirement = declarations
                .iter()
                .map(|dep| dep.req.as_str())
                .collect::<BTreeSet<_>>();
            let optional = declarations.iter().all(|dep| dep.optional);
            entries.push(CollectedDependencyEntry {
                local: self.is_local_package(dep_package),
                name: dep_package.name.clone(),
                requirement: join_str_set(&requirement),
                kind: kind.to_string(),
                optional,
            });
        }
        entries
    }

    fn collect_declared_dependency_specs(
        &mut self,
        include_optional: bool,
        selection: DeclaredDependencySelection,
    ) -> Result<()> {
        for package_id in self.package_ids.clone() {
            let package = self.package(package_id)?;
            let entries = package
                .dependencies
                .iter()
                .filter_map(|dep| self.declared_dependency_entry(dep, include_optional, selection))
                .collect::<Vec<_>>();

            for entry in entries {
                self.insert_entry(entry);
            }
        }
        Ok(())
    }

    fn declared_dependency_entry(
        &self,
        dep: &CargoDependency,
        include_optional: bool,
        selection: DeclaredDependencySelection,
    ) -> Option<CollectedDependencyEntry> {
        if !selection.matches(dep.optional) {
            return None;
        }
        if dep.optional && !include_optional {
            return None;
        }

        let kind = dependency_kind(dep.kind.as_deref());
        if !should_include_kind(kind, self.include_dev, self.include_build) {
            return None;
        }

        Some(CollectedDependencyEntry {
            local: self.declared_dependency_is_local(dep),
            name: dep.name.clone(),
            requirement: dep.req.clone(),
            kind: kind.to_string(),
            optional: dep.optional,
        })
    }

    fn insert_entry(&mut self, entry: CollectedDependencyEntry) {
        Self::insert_entry_into(&mut self.collected, &mut self.skipped_local, entry);
    }

    fn insert_entry_into(
        collected: &mut BTreeMap<String, DependencySpecBuilder>,
        skipped_local: &mut BTreeMap<String, DependencySpecBuilder>,
        entry: CollectedDependencyEntry,
    ) {
        let target = if entry.local {
            skipped_local
        } else {
            collected
        };
        insert_dependency_spec(
            target,
            entry.name,
            entry.requirement,
            entry.kind,
            entry.optional,
        );
    }

    fn package(&self, package_id: &str) -> Result<&'a CargoPackage> {
        self.package_by_id
            .get(package_id)
            .copied()
            .ok_or_else(|| anyhow!("workspace package id not found in metadata: {package_id}"))
    }

    fn is_local_package(&self, package: &CargoPackage) -> bool {
        package.source.is_none() || self.workspace_member_ids.contains(package.id.as_str())
    }

    fn declared_dependency_is_local(&self, dep: &CargoDependency) -> bool {
        // Manifest parsing leaves `source` empty for path, workspace-local, and git deps.
        // Registry dependencies carry a concrete source even when they are inactive optional deps.
        dep.source.is_none()
    }

    fn finish(self) -> DependencyInventory {
        DependencyInventory {
            specs: build_dependency_specs(self.collected),
            skipped_local: build_dependency_specs(self.skipped_local),
        }
    }
}

fn build_dependency_specs(
    collected: BTreeMap<String, DependencySpecBuilder>,
) -> Vec<DependencySpec> {
    let mut out = Vec::with_capacity(collected.len());
    for (name, builder) in collected {
        out.push(DependencySpec {
            name,
            requirement: join_set(&builder.requirements),
            kinds: join_set(&builder.kinds),
            optional: builder.optional,
        });
    }
    out
}

fn insert_dependency_spec(
    collected: &mut BTreeMap<String, DependencySpecBuilder>,
    name: String,
    requirement: String,
    kind: String,
    optional: bool,
) {
    let entry = collected.entry(name).or_default();
    entry.requirements.insert(requirement);
    entry.kinds.insert(kind);
    entry.optional = entry.optional && optional;
}

fn matching_dependency_declarations<'a>(
    package: &'a CargoPackage,
    dep_package_name: &str,
    kind: &str,
) -> Vec<&'a CargoDependency> {
    let mut matches = package
        .dependencies
        .iter()
        .filter(|dep| {
            dependency_name_matches(dep.name.as_str(), dep_package_name)
                && dependency_kind(dep.kind.as_deref()) == kind
        })
        .collect::<Vec<_>>();

    if matches.is_empty() {
        matches = package
            .dependencies
            .iter()
            .filter(|dep| dependency_name_matches(dep.name.as_str(), dep_package_name))
            .collect::<Vec<_>>();
    }

    matches
}

fn dependency_kinds_from_resolve(dep: &CargoResolveNodeDep) -> BTreeSet<&str> {
    let mut kinds = BTreeSet::new();
    for dep_kind in &dep.dep_kinds {
        kinds.insert(dependency_kind(dep_kind.kind.as_deref()));
    }
    if kinds.is_empty() {
        kinds.insert("normal");
    }
    kinds
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

fn dependency_name_matches(left: &str, right: &str) -> bool {
    left == right || normalize_dependency_name(left) == normalize_dependency_name(right)
}

fn normalize_dependency_name(name: &str) -> String {
    name.replace('-', "_")
}

fn join_str_set(set: &BTreeSet<&str>) -> String {
    set.iter().copied().collect::<Vec<_>>().join(",")
}

fn target_package_ids(metadata: &CargoMetadata) -> Vec<&str> {
    if !metadata.workspace_members.is_empty() {
        return metadata
            .workspace_members
            .iter()
            .map(String::as_str)
            .collect();
    }
    metadata.root.as_deref().into_iter().collect()
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

#[derive(Debug, Clone)]
struct WorkflowActionSpec {
    action: String,
    owner: String,
    repo: String,
    path: Option<String>,
    ref_name: String,
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
    let value = normalize_workflow_uses_value(raw_value)?;
    if value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with("docker://")
        || value.starts_with("${{")
    {
        return None;
    }

    let (action, ref_name) = value.rsplit_once('@')?;
    if action.is_empty() || ref_name.is_empty() {
        return None;
    }
    let mut parts = action.split('/');
    let owner = valid_action_segment(parts.next()?)?;
    let repo = valid_action_segment(parts.next()?)?;
    let rest = parts.collect::<Vec<_>>();
    if rest.iter().any(|part| valid_action_segment(part).is_none()) {
        return None;
    }

    Some(WorkflowActionSpec {
        action: action.to_string(),
        owner: owner.to_string(),
        repo: repo.to_string(),
        path: (!rest.is_empty()).then(|| rest.join("/")),
        ref_name: ref_name.to_string(),
        locations: Vec::new(),
    })
}

fn normalize_workflow_uses_value(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let without_comment = trimmed
        .split_once(" #")
        .map(|(value, _)| value)
        .unwrap_or(trimmed)
        .trim();
    let value = strip_matching_quotes(without_comment).unwrap_or(without_comment);
    (!value.is_empty()).then(|| value.to_string())
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
    latest_tags: std::result::Result<Vec<String>, String>,
) -> ActionAuditRecord {
    let mut record = ActionAuditRecord {
        action: spec.action,
        owner: spec.owner,
        repo: spec.repo,
        path: spec.path,
        current_ref: spec.ref_name,
        latest_ref: None,
        update_plan: ActionUpdatePlan::Review,
        note: None,
        locations: spec.locations,
    };

    if is_full_commit_sha(&record.current_ref) {
        record.update_plan = ActionUpdatePlan::Keep;
        record.note = Some("sha-pinned".to_string());
        return record;
    }

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
    let Some(current) = parse_action_tag_version(&record.current_ref) else {
        record.note = Some("floating or non-semver ref; review manually".to_string());
        return record;
    };

    if current_action_ref_is_outdated(&current, &latest) {
        record.update_plan = ActionUpdatePlan::Bump;
        record.note = Some("newer action tag available".to_string());
    } else {
        record.update_plan = ActionUpdatePlan::Keep;
        record.note = Some("current ref is up to date".to_string());
    }
    record
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct ActionTagVersion {
    tag: String,
    version: Version,
    precision: ActionTagPrecision,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ActionTagPrecision {
    Major,
    Minor,
    Patch,
}

fn latest_stable_action_tag(tags: &[String]) -> Option<ActionTagVersion> {
    tags.iter()
        .filter_map(|tag| parse_action_tag_version(tag))
        .filter(|version| version.version.pre.is_empty())
        .max_by(|a, b| a.version.cmp(&b.version).then_with(|| a.tag.cmp(&b.tag)))
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
        .any(|part| part.bytes().any(|b| !b.is_ascii_digit()))
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
    value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit())
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
}

#[derive(Debug, Deserialize)]
struct ManifestPackage {
    name: String,
}

#[derive(Debug, Deserialize, Default)]
struct ManifestWorkspace {
    #[serde(default)]
    members: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default, rename = "dependencies")]
    dependencies: BTreeMap<String, ManifestDependency>,
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
            workspace: false,
            optional: self.optional.or(workspace.optional),
        }
    }
}

struct ResolvedManifestDependency {
    name: String,
    req: String,
    crates_io: bool,
    optional: bool,
}

#[derive(Debug, Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoPackage>,
    workspace_members: Vec<String>,
    root: Option<String>,
    #[serde(default)]
    workspace_root: Option<PathBuf>,
    resolve: Option<CargoResolve>,
}

#[derive(Debug, Deserialize)]
struct CargoPackage {
    id: String,
    name: String,
    #[serde(default)]
    source: Option<String>,
    dependencies: Vec<CargoDependency>,
}

#[derive(Debug, Deserialize)]
struct CargoDependency {
    name: String,
    #[serde(default)]
    source: Option<String>,
    req: String,
    kind: Option<String>,
    optional: bool,
}

#[derive(Debug, Deserialize)]
struct CargoResolve {
    nodes: Vec<CargoResolveNode>,
}

#[derive(Debug, Deserialize)]
struct CargoResolveNode {
    id: String,
    #[serde(default)]
    deps: Vec<CargoResolveNodeDep>,
}

#[derive(Debug, Deserialize)]
struct CargoResolveNodeDep {
    pkg: String,
    #[serde(default)]
    dep_kinds: Vec<CargoResolveDepKind>,
}

#[derive(Debug, Deserialize)]
struct CargoResolveDepKind {
    kind: Option<String>,
}

#[derive(Clone, Copy)]
enum DeclaredDependencySelection {
    All,
    OptionalOnly,
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

impl DeclaredDependencySelection {
    fn matches(self, optional: bool) -> bool {
        match self {
            Self::All => true,
            Self::OptionalOnly => optional,
        }
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

#[derive(Debug, Clone, Deserialize)]
struct GitHubTagResponse {
    name: String,
}

#[cfg(test)]
mod tests;
