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
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    env, fs,
    io::IsTerminal,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, SystemTime},
};

use self::api::ApiClient;
use self::model::{
    AuditReport, AuditSummary, DepAuditRecord, DependencySpec, DependencySpecBuilder,
    DependencyUpdatePlan, GitHubCacheEntry, RiskLevel, age_days_from_now, classify_risk,
    github_repo_from_url, std_alternative,
};
use self::render::{build_summary, print_report, write_json_report};

const HTTP_TIMEOUT_SECS: u64 = 30;
const HTTP_USER_AGENT: &str = "za-deps-audit/0.1";
const HTTP_MAX_ATTEMPTS: usize = 3;
const HTTP_BACKOFF_BASE_MS: u64 = 200;
const AUTO_DEPS_JOBS_MULTIPLIER: usize = 2;
const AUTO_DEPS_JOBS_MIN: usize = 4;
const AUTO_DEPS_JOBS_MAX: usize = 16;
const DEPS_CACHE_SCHEMA_VERSION: u32 = 1;
const DEPS_CACHE_FILE_NAME: &str = "deps-cache-v1.json";
const CRATES_CACHE_TTL_SECS: u64 = 6 * 60 * 60;
const GITHUB_CACHE_TTL_SECS: u64 = 60 * 60;

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
    let metadata = cargo_metadata(&manifest_path)?;
    let inventory =
        collect_dependency_inventory(&metadata, include_dev, include_build, include_optional)?;
    if inventory.specs.is_empty() {
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
    let worker_count = normalize_jobs(requested_jobs, inventory.specs.len());
    println!(
        "Auditing {} dependencies with {} workers...",
        inventory.specs.len(),
        worker_count
    );
    let skipped_local = inventory.skipped_local_count();
    let api = Arc::new(ApiClient::new(github_token_override)?);
    let mut records = audit_dependencies(Arc::clone(&api), inventory.specs, worker_count)?;
    sort_records(&mut records);

    let summary = build_summary(&records, skipped_local);
    print_report(&manifest_path, &summary, &records, verbose);

    if let Some(path) = json_out {
        write_json_report(path, &manifest_path, &summary, &records)?;
    }

    api.flush_cache().context("flush dependency audit cache")?;

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

fn cargo_metadata(manifest_path: &Path) -> Result<CargoMetadata> {
    let output = Command::new("cargo")
        .arg("metadata")
        .arg("--format-version")
        .arg("1")
        .arg("--manifest-path")
        .arg(manifest_path)
        .output()
        .context("run `cargo metadata`")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("`cargo metadata` failed: {}", stderr.trim());
    }
    serde_json::from_slice::<CargoMetadata>(&output.stdout)
        .context("parse `cargo metadata` JSON output")
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
        // `cargo metadata` leaves declaration `source` empty for path/workspace dependencies.
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

fn join_set(set: &BTreeSet<String>) -> String {
    set.iter().cloned().collect::<Vec<_>>().join(",")
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

#[derive(Debug, Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoPackage>,
    workspace_members: Vec<String>,
    root: Option<String>,
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

#[cfg(test)]
mod tests;
