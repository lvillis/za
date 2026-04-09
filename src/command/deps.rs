//! Dependency governance and maintenance audit for Rust projects.

mod api;
mod model;

use crate::command::{style as tty_style, za_config};
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
    GitHubCacheEntry, RiskLevel, age_days_from_now, classify_risk, github_repo_from_url,
    std_alternative,
};

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
    pub jobs: Option<usize>,
    pub include_dev: bool,
    pub include_build: bool,
    pub include_optional: bool,
    pub json: bool,
    pub toml: bool,
}

#[derive(Debug, Clone)]
struct LatestQuery {
    name: String,
    requirement: Option<String>,
    kinds: Option<String>,
    source: LatestQuerySource,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum LatestQuerySource {
    Args,
    Manifest,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum LatestStatus {
    Resolved,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
struct LatestRecord {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    requirement: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kinds: Option<String>,
    source: LatestQuerySource,
    status: LatestStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    latest_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
struct LatestSummary {
    total: usize,
    resolved: usize,
    failed: usize,
}

#[derive(Debug, Serialize)]
struct LatestReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    manifest_path: Option<String>,
    summary: LatestSummary,
    records: Vec<LatestRecord>,
}

pub fn run(opts: DepsRunOptions) -> Result<()> {
    let DepsRunOptions {
        manifest_path,
        github_token_override,
        jobs,
        include_dev,
        include_build,
        include_optional,
        json_out,
        fail_on_high,
        verbose,
    } = opts;

    let manifest_path = canonical_manifest_path(manifest_path)?;
    let metadata = cargo_metadata(&manifest_path)?;
    let specs = collect_dependency_specs(&metadata, include_dev, include_build, include_optional)?;
    if specs.is_empty() {
        println!("No dependencies found for audit.");
        return Ok(());
    }

    let requested_jobs = jobs.unwrap_or_else(default_deps_jobs);
    let worker_count = normalize_jobs(requested_jobs, specs.len());
    println!(
        "Auditing {} dependencies with {} workers...",
        specs.len(),
        worker_count
    );
    let api = Arc::new(ApiClient::new(github_token_override)?);
    let mut records = audit_dependencies(Arc::clone(&api), specs, worker_count)?;
    sort_records(&mut records);

    let summary = build_summary(&records);
    print_report(&manifest_path, &summary, &records, verbose);

    if let Some(path) = json_out {
        write_json_report(path, &manifest_path, &summary, &records)?;
    }

    let _ = api.flush_cache();

    if fail_on_high && summary.high > 0 {
        bail!("dependency audit found {} high-risk entries", summary.high);
    }
    Ok(())
}

pub fn run_latest(opts: DepsLatestOptions) -> Result<()> {
    let DepsLatestOptions {
        crates,
        manifest_path,
        jobs,
        include_dev,
        include_build,
        include_optional,
        json,
        toml,
    } = opts;

    let (manifest_path, queries) = collect_latest_queries(
        crates,
        manifest_path,
        include_dev,
        include_build,
        include_optional,
    )?;
    if queries.is_empty() {
        bail!("provide crate names or `--manifest-path <Cargo.toml>`");
    }

    let requested_jobs = jobs.unwrap_or_else(default_deps_jobs);
    let worker_count = normalize_jobs(requested_jobs, queries.len());
    if !json && !toml {
        println!(
            "Resolving latest stable versions for {} crate(s) with {} workers...",
            queries.len(),
            worker_count
        );
    }

    let api = Arc::new(ApiClient::new(None)?);
    let mut records = resolve_latest_records(Arc::clone(&api), queries, worker_count)?;
    records.sort_by(|a, b| a.name.cmp(&b.name));
    let summary = build_latest_summary(&records);

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&LatestReport {
                manifest_path: manifest_path
                    .as_ref()
                    .map(|path| path.display().to_string()),
                summary: summary.clone(),
                records: records.clone(),
            })
            .context("serialize latest dependency output")?
        );
    } else if toml {
        print!("{}", render_latest_toml(&records));
    } else {
        for line in render_latest_lines(manifest_path.as_deref(), &summary, &records) {
            println!("{line}");
        }
    }

    let _ = api.flush_cache();
    Ok(())
}

fn normalize_jobs(requested_jobs: usize, deps_count: usize) -> usize {
    requested_jobs.max(1).min(deps_count.max(1))
}

fn collect_latest_queries(
    crates: Vec<String>,
    manifest_path: Option<PathBuf>,
    include_dev: bool,
    include_build: bool,
    include_optional: bool,
) -> Result<(Option<PathBuf>, Vec<LatestQuery>)> {
    let mut queries = BTreeMap::<String, LatestQuery>::new();
    let manifest_path = match manifest_path {
        Some(path) => {
            let manifest_path = canonical_manifest_path(Some(path))?;
            let metadata = cargo_metadata(&manifest_path)?;
            let specs =
                collect_dependency_specs(&metadata, include_dev, include_build, include_optional)?;
            for spec in specs {
                let key = normalize_dependency_name(&spec.name);
                queries
                    .entry(key)
                    .and_modify(|query| {
                        if query.requirement.is_none() && !spec.requirement.is_empty() {
                            query.requirement = Some(spec.requirement.clone());
                        }
                        if query.kinds.is_none() && !spec.kinds.is_empty() {
                            query.kinds = Some(spec.kinds.clone());
                        }
                        query.source = LatestQuerySource::Manifest;
                    })
                    .or_insert_with(|| LatestQuery {
                        name: spec.name,
                        requirement: Some(spec.requirement),
                        kinds: Some(spec.kinds),
                        source: LatestQuerySource::Manifest,
                    });
            }
            Some(manifest_path)
        }
        None => None,
    };

    for krate in crates {
        let trimmed = krate.trim();
        if trimmed.is_empty() {
            continue;
        }
        let key = normalize_dependency_name(trimmed);
        queries.entry(key).or_insert_with(|| LatestQuery {
            name: trimmed.to_string(),
            requirement: None,
            kinds: None,
            source: LatestQuerySource::Args,
        });
    }

    Ok((manifest_path, queries.into_values().collect()))
}

fn resolve_latest_records(
    api: Arc<ApiClient>,
    queries: Vec<LatestQuery>,
    jobs: usize,
) -> Result<Vec<LatestRecord>> {
    let progress = build_progress(queries.len() as u64);
    let queue = Arc::new(Mutex::new(VecDeque::from(queries)));
    let records = Arc::new(Mutex::new(Vec::new()));
    let first_error: Arc<Mutex<Option<anyhow::Error>>> = Arc::new(Mutex::new(None));

    thread::scope(|scope| {
        for _ in 0..jobs {
            let api = Arc::clone(&api);
            let queue = Arc::clone(&queue);
            let records = Arc::clone(&records);
            let first_error = Arc::clone(&first_error);
            let progress = progress.clone();

            scope.spawn(move || {
                loop {
                    if has_error(first_error.as_ref()) {
                        break;
                    }

                    let query = match queue.lock() {
                        Ok(mut guard) => guard.pop_front(),
                        Err(_) => {
                            store_error(
                                first_error.as_ref(),
                                anyhow!("latest version queue lock poisoned"),
                            );
                            break;
                        }
                    };

                    let Some(query) = query else {
                        break;
                    };
                    let record = resolve_latest_record(api.as_ref(), query);
                    match records.lock() {
                        Ok(mut guard) => guard.push(record),
                        Err(_) => {
                            store_error(
                                first_error.as_ref(),
                                anyhow!("latest version records lock poisoned"),
                            );
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
        .map_err(|_| anyhow!("latest version records lock poisoned"))?;
    Ok(std::mem::take(&mut *records_guard))
}

fn resolve_latest_record(api: &ApiClient, query: LatestQuery) -> LatestRecord {
    match api.fetch_crate(&query.name) {
        Ok(snapshot) => {
            let mut notes = Vec::new();
            if snapshot.latest_version_yanked == Some(true) {
                notes.push("latest stable is yanked".to_string());
            }
            if let Some(rust_version) = snapshot.latest_version_rust_version.as_deref() {
                notes.push(format!("rust {rust_version}"));
            }
            LatestRecord {
                name: query.name,
                requirement: query.requirement,
                kinds: query.kinds,
                source: query.source,
                status: LatestStatus::Resolved,
                latest_version: Some(snapshot.max_version),
                note: (!notes.is_empty()).then(|| notes.join("; ")),
            }
        }
        Err(err) => LatestRecord {
            name: query.name,
            requirement: query.requirement,
            kinds: query.kinds,
            source: query.source,
            status: LatestStatus::Failed,
            latest_version: None,
            note: Some(format!("crates.io query failed: {err}")),
        },
    }
}

fn build_latest_summary(records: &[LatestRecord]) -> LatestSummary {
    let mut summary = LatestSummary {
        total: records.len(),
        ..Default::default()
    };
    for record in records {
        match record.status {
            LatestStatus::Resolved => summary.resolved += 1,
            LatestStatus::Failed => summary.failed += 1,
        }
    }
    summary
}

fn render_latest_lines(
    manifest_path: Option<&Path>,
    summary: &LatestSummary,
    records: &[LatestRecord],
) -> Vec<String> {
    let verdict = if summary.failed > 0 {
        tty_style::warning(format!("{:<5}", "WARN"))
    } else {
        tty_style::success(format!("{:<5}", "OK"))
    };
    let mut lines = vec![format!(
        "{} {}  {} {}  {}",
        verdict,
        tty_style::header("latest"),
        tty_style::header(summary.total.to_string()),
        tty_style::dim("crates"),
        render_latest_summary(summary)
    )];

    if records.is_empty() {
        return lines;
    }

    let name_width = records
        .iter()
        .map(|record| record.name.chars().count())
        .max()
        .unwrap_or(4)
        .clamp(4, 28);
    let req_width = records
        .iter()
        .map(|record| record.requirement.as_deref().unwrap_or("-").chars().count())
        .max()
        .unwrap_or(3)
        .clamp(3, 20);
    let latest_width = records
        .iter()
        .map(|record| {
            record
                .latest_version
                .as_deref()
                .unwrap_or("-")
                .chars()
                .count()
        })
        .max()
        .unwrap_or(6)
        .clamp(6, 20);
    let kinds_width = records
        .iter()
        .map(|record| record.kinds.as_deref().unwrap_or("-").chars().count())
        .max()
        .unwrap_or(5)
        .clamp(5, 16);

    lines.push(String::new());
    lines.push(tty_style::dim(format!(
        "{:<5}  {:<name_width$}  {:<req_width$}  {:<latest_width$}  {:<kinds_width$}  note",
        "st", "name", "req", "latest", "kinds"
    )));
    for record in records {
        lines.push(format!(
            "{}  {:<name_width$}  {:<req_width$}  {:<latest_width$}  {:<kinds_width$}  {}",
            style_latest_status(record.status),
            truncate(&record.name, name_width),
            truncate(record.requirement.as_deref().unwrap_or("-"), req_width),
            style_latest_version_cell(
                &truncate(
                    record.latest_version.as_deref().unwrap_or("-"),
                    latest_width
                ),
                latest_width,
                record.status,
            ),
            tty_style::dim(format!(
                "{:<kinds_width$}",
                truncate(record.kinds.as_deref().unwrap_or("-"), kinds_width)
            )),
            truncate(
                record.note.as_deref().unwrap_or(match record.source {
                    LatestQuerySource::Args => "explicit query",
                    LatestQuerySource::Manifest => "manifest",
                }),
                96
            )
        ));
    }

    if let Some(path) = manifest_path {
        lines.push(String::new());
        lines.push(format!(
            "{}  {}",
            tty_style::dim("manifest"),
            path.display()
        ));
    }

    lines
}

fn render_latest_toml(records: &[LatestRecord]) -> String {
    let mut out = String::new();
    for record in records {
        match record.latest_version.as_deref() {
            Some(version) => {
                out.push_str(&format!("{} = \"{}\"\n", record.name, version));
            }
            None => {
                out.push_str("# ");
                out.push_str(&record.name);
                out.push_str(": ");
                out.push_str(
                    record
                        .note
                        .as_deref()
                        .unwrap_or("latest version unavailable"),
                );
                out.push('\n');
            }
        }
    }
    out
}

fn render_latest_summary(summary: &LatestSummary) -> String {
    let mut parts = Vec::new();
    if summary.resolved > 0 {
        parts.push(tty_style::success(format!("{} resolved", summary.resolved)));
    }
    if summary.failed > 0 {
        parts.push(tty_style::warning(format!("{} failed", summary.failed)));
    }
    if parts.is_empty() {
        tty_style::dim("no results")
    } else {
        parts.join(&format!(" {} ", tty_style::dim("·")))
    }
}

fn style_latest_status(status: LatestStatus) -> String {
    match status {
        LatestStatus::Resolved => tty_style::success(format!("{:<5}", "OK")),
        LatestStatus::Failed => tty_style::warning(format!("{:<5}", "WARN")),
    }
}

fn style_latest_version_cell(value: &str, width: usize, status: LatestStatus) -> String {
    let padded = format!("{value:<width$}");
    match status {
        LatestStatus::Resolved => tty_style::active(padded),
        LatestStatus::Failed => tty_style::dim(padded),
    }
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
    let progress = build_progress(specs.len() as u64);
    let queue = Arc::new(Mutex::new(VecDeque::from(specs)));
    let records = Arc::new(Mutex::new(Vec::new()));
    let first_error: Arc<Mutex<Option<anyhow::Error>>> = Arc::new(Mutex::new(None));

    thread::scope(|scope| {
        for _ in 0..jobs {
            let api = Arc::clone(&api);
            let queue = Arc::clone(&queue);
            let records = Arc::clone(&records);
            let first_error = Arc::clone(&first_error);
            let progress = progress.clone();

            scope.spawn(move || {
                loop {
                    if has_error(first_error.as_ref()) {
                        break;
                    }

                    let spec = match queue.lock() {
                        Ok(mut guard) => guard.pop_front(),
                        Err(_) => {
                            store_error(
                                first_error.as_ref(),
                                anyhow!("dependency queue lock poisoned"),
                            );
                            break;
                        }
                    };

                    let Some(spec) = spec else {
                        break;
                    };

                    match api.audit_one(spec) {
                        Ok(record) => match records.lock() {
                            Ok(mut guard) => guard.push(record),
                            Err(_) => {
                                store_error(
                                    first_error.as_ref(),
                                    anyhow!("dependency records lock poisoned"),
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
        .map_err(|_| anyhow!("dependency records lock poisoned"))?;
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

fn canonical_manifest_path(input: Option<PathBuf>) -> Result<PathBuf> {
    let path = match input {
        Some(path) => path,
        None => PathBuf::from("Cargo.toml"),
    };
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
    let mut package_by_id: BTreeMap<&str, &CargoPackage> = BTreeMap::new();
    for pkg in &metadata.packages {
        package_by_id.insert(pkg.id.as_str(), pkg);
    }

    let package_ids = target_package_ids(metadata);
    let mut collected: BTreeMap<String, DependencySpecBuilder> = BTreeMap::new();

    let used_resolve = collect_resolved_dependency_specs(
        metadata,
        &package_by_id,
        &package_ids,
        include_dev,
        include_build,
        &mut collected,
    )?;

    if include_optional {
        collect_declared_dependency_specs(
            &package_by_id,
            &package_ids,
            include_dev,
            include_build,
            true,
            &mut collected,
            DeclaredDependencySelection::OptionalOnly,
        )?;
    }

    if !used_resolve {
        collect_declared_dependency_specs(
            &package_by_id,
            &package_ids,
            include_dev,
            include_build,
            include_optional,
            &mut collected,
            DeclaredDependencySelection::All,
        )?;
    }

    Ok(build_dependency_specs(collected))
}

fn collect_resolved_dependency_specs(
    metadata: &CargoMetadata,
    package_by_id: &BTreeMap<&str, &CargoPackage>,
    package_ids: &[&str],
    include_dev: bool,
    include_build: bool,
    collected: &mut BTreeMap<String, DependencySpecBuilder>,
) -> Result<bool> {
    let Some(resolve) = metadata.resolve.as_ref() else {
        return Ok(false);
    };

    let mut node_by_id: BTreeMap<&str, &CargoResolveNode> = BTreeMap::new();
    for node in &resolve.nodes {
        node_by_id.insert(node.id.as_str(), node);
    }

    for package_id in package_ids {
        let package = package_by_id
            .get(package_id)
            .ok_or_else(|| anyhow!("workspace package id not found in metadata: {package_id}"))?;
        let Some(node) = node_by_id.get(package_id) else {
            return Ok(false);
        };

        for dep in &node.deps {
            let dep_package = package_by_id
                .get(dep.pkg.as_str())
                .ok_or_else(|| anyhow!("resolved dependency package not found: {}", dep.pkg))?;

            let active_kinds = dependency_kinds_from_resolve(dep);
            for kind in active_kinds {
                if !should_include_kind(kind, include_dev, include_build) {
                    continue;
                }

                let declarations =
                    matching_dependency_declarations(package, &dep_package.name, kind);
                let requirement = declarations
                    .iter()
                    .map(|dep| dep.req.as_str())
                    .collect::<BTreeSet<_>>();
                let optional = declarations.iter().all(|dep| dep.optional);

                insert_dependency_spec(
                    collected,
                    dep_package.name.clone(),
                    join_str_set(&requirement),
                    kind.to_string(),
                    optional,
                );
            }
        }
    }

    Ok(true)
}

fn collect_declared_dependency_specs(
    package_by_id: &BTreeMap<&str, &CargoPackage>,
    package_ids: &[&str],
    include_dev: bool,
    include_build: bool,
    include_optional: bool,
    collected: &mut BTreeMap<String, DependencySpecBuilder>,
    selection: DeclaredDependencySelection,
) -> Result<()> {
    for package_id in package_ids {
        let package = package_by_id
            .get(package_id)
            .ok_or_else(|| anyhow!("workspace package id not found in metadata: {package_id}"))?;

        for dep in &package.dependencies {
            if !selection.matches(dep.optional) {
                continue;
            }
            if dep.optional && !include_optional {
                continue;
            }

            let kind = dependency_kind(dep.kind.as_deref());
            if !should_include_kind(kind, include_dev, include_build) {
                continue;
            }

            insert_dependency_spec(
                collected,
                dep.name.clone(),
                dep.req.clone(),
                kind.to_string(),
                dep.optional,
            );
        }
    }

    Ok(())
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
    if let Some(root) = metadata.root.as_deref() {
        return vec![root];
    }
    metadata
        .workspace_members
        .iter()
        .map(String::as_str)
        .collect()
}

fn join_set(set: &BTreeSet<String>) -> String {
    set.iter().cloned().collect::<Vec<_>>().join(",")
}

fn sort_records(records: &mut [DepAuditRecord]) {
    records.sort_by(|a, b| {
        b.risk
            .weight()
            .cmp(&a.risk.weight())
            .then_with(|| a.name.cmp(&b.name))
    });
}

fn build_summary(records: &[DepAuditRecord]) -> AuditSummary {
    let mut summary = AuditSummary::default();
    for rec in records {
        match rec.risk {
            RiskLevel::High => summary.high += 1,
            RiskLevel::Medium => summary.medium += 1,
            RiskLevel::Low => summary.low += 1,
            RiskLevel::Unknown => summary.unknown += 1,
        }
    }
    summary
}

fn print_report(
    manifest_path: &Path,
    summary: &AuditSummary,
    records: &[DepAuditRecord],
    verbose: bool,
) {
    for line in render_report_lines(manifest_path, summary, records, verbose) {
        println!("{line}");
    }
}

fn render_report_lines(
    manifest_path: &Path,
    summary: &AuditSummary,
    records: &[DepAuditRecord],
    verbose: bool,
) -> Vec<String> {
    let mut lines = vec![render_report_summary_line(
        manifest_path,
        summary,
        records.len(),
    )];
    let attention = records
        .iter()
        .filter(|record| record.risk != RiskLevel::Low)
        .collect::<Vec<_>>();
    let low = records
        .iter()
        .filter(|record| record.risk == RiskLevel::Low)
        .collect::<Vec<_>>();

    if !attention.is_empty() {
        lines.push(String::new());
        lines.push(tty_style::header("attention"));
        lines.extend(render_record_table(&attention));
    }

    if verbose {
        if !low.is_empty() {
            lines.push(String::new());
            lines.push(tty_style::header("baseline"));
            lines.extend(render_record_table(&low));
        }
        lines.push(String::new());
        lines.push(format!(
            "{}  {}",
            tty_style::dim("manifest"),
            manifest_path.display()
        ));
    } else if !low.is_empty() {
        lines.push(String::new());
        lines.push(format!(
            "{}       {} low-risk entr{} hidden; rerun with `za deps --verbose` for the full inventory",
            tty_style::dim("low"),
            low.len(),
            if low.len() == 1 { "y is" } else { "ies are" }
        ));
    }

    lines
}

fn render_report_summary_line(
    manifest_path: &Path,
    summary: &AuditSummary,
    total: usize,
) -> String {
    let manifest = manifest_path
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| manifest_path.to_string_lossy().into_owned());
    let verdict = style_report_verdict(&format!("{:<5}", report_verdict(summary)), summary);
    format!(
        "{}  {}  {} {}  {}",
        verdict,
        tty_style::header(manifest),
        tty_style::header(total.to_string()),
        tty_style::dim("deps"),
        render_summary_counts(summary)
    )
}

fn report_verdict(summary: &AuditSummary) -> &'static str {
    if summary.high > 0 {
        "HIGH"
    } else if summary.medium > 0 {
        "MED"
    } else if summary.unknown > 0 {
        "WARN"
    } else {
        "OK"
    }
}

fn render_summary_counts(summary: &AuditSummary) -> String {
    let mut parts = Vec::new();
    if summary.high > 0 {
        parts.push(tty_style::error(format!("{} high", summary.high)));
    }
    if summary.medium > 0 {
        parts.push(tty_style::warning(format!("{} medium", summary.medium)));
    }
    if summary.unknown > 0 {
        parts.push(tty_style::active(format!("{} unknown", summary.unknown)));
    }
    if summary.low > 0 {
        parts.push(tty_style::dim(format!("{} low", summary.low)));
    }
    if parts.is_empty() {
        tty_style::dim("no findings")
    } else {
        parts.join(&format!(" {} ", tty_style::dim("·")))
    }
}

fn render_record_table(records: &[&DepAuditRecord]) -> Vec<String> {
    let name_width = column_width(records, "name", |record| &record.name, 24);
    let req_width = column_width(records, "req", |record| &record.requirement, 16);
    let latest_width = column_width(
        records,
        "latest",
        |record| record.latest_version.as_deref().unwrap_or("-"),
        14,
    );
    let kinds_width = column_width(records, "kinds", |record| &record.kinds, 12);

    let mut lines = Vec::with_capacity(records.len() + 1);
    lines.push(tty_style::dim(format!(
        "{:<5}  {:<name_width$}  {:<req_width$}  {:<latest_width$}  {:<kinds_width$}  note",
        "risk", "name", "req", "latest", "kinds",
    )));

    for record in records {
        let risk = style_record_risk(
            &format!("{:<5}", record_risk_label(record.risk)),
            record.risk,
        );
        let name = style_dep_name_cell(&truncate(&record.name, name_width), name_width);
        let requirement = tty_style::dim(format!(
            "{:<req_width$}",
            truncate(&record.requirement, req_width)
        ));
        let latest = style_dep_latest_cell(
            &truncate(
                record.latest_version.as_deref().unwrap_or("-"),
                latest_width,
            ),
            latest_width,
            record.latest_version.is_some(),
        );
        let kinds = tty_style::dim(format!(
            "{:<kinds_width$}",
            truncate(&record.kinds, kinds_width)
        ));
        lines.push(format!(
            "{}  {}  {}  {}  {}  {}",
            risk,
            name,
            requirement,
            latest,
            kinds,
            summarize_record_note(record),
        ));
    }

    lines
}

fn column_width<'a, F>(
    records: &[&'a DepAuditRecord],
    header: &str,
    value: F,
    max_width: usize,
) -> usize
where
    F: Fn(&'a DepAuditRecord) -> &'a str,
{
    records
        .iter()
        .map(|record| value(record).chars().count())
        .max()
        .unwrap_or(header.chars().count())
        .max(header.chars().count())
        .min(max_width)
}

fn record_risk_label(risk: RiskLevel) -> &'static str {
    match risk {
        RiskLevel::High => "HIGH",
        RiskLevel::Medium => "MED",
        RiskLevel::Low => "LOW",
        RiskLevel::Unknown => "WARN",
    }
}

fn summarize_record_note(record: &DepAuditRecord) -> String {
    if record.notes.is_empty() {
        return tty_style::dim("-");
    }
    truncate(&record.notes.join("; "), 96)
}

fn style_dep_name_cell(value: &str, width: usize) -> String {
    tty_style::header(format!("{value:<width$}"))
}

fn style_dep_latest_cell(value: &str, width: usize, has_latest: bool) -> String {
    let padded = format!("{value:<width$}");
    if has_latest {
        tty_style::active(padded)
    } else {
        tty_style::dim(padded)
    }
}

fn style_report_verdict(value: &str, summary: &AuditSummary) -> String {
    if summary.high > 0 {
        tty_style::error(value)
    } else if summary.medium > 0 {
        tty_style::warning(value)
    } else if summary.unknown > 0 {
        tty_style::active(value)
    } else {
        tty_style::success(value)
    }
}

fn style_record_risk(value: &str, risk: RiskLevel) -> String {
    match risk {
        RiskLevel::High => tty_style::error(value),
        RiskLevel::Medium => tty_style::warning(value),
        RiskLevel::Low => tty_style::dim(value),
        RiskLevel::Unknown => tty_style::active(value),
    }
}

fn write_json_report(
    path: PathBuf,
    manifest_path: &Path,
    summary: &AuditSummary,
    records: &[DepAuditRecord],
) -> Result<()> {
    let report = AuditReport {
        generated_at: format_rfc3339_seconds(SystemTime::now()).to_string(),
        manifest_path: manifest_path.display().to_string(),
        summary: summary.clone(),
        dependencies: records.to_vec(),
    };
    let json = serde_json::to_vec_pretty(&report).context("serialize dependency report JSON")?;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("create report directory {}", parent.display()))?;
    }
    fs::write(&path, json).with_context(|| format!("write {}", path.display()))?;
    println!("JSON report written: {}", path.display());
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out = String::new();
    for c in s.chars().take(max.saturating_sub(1)) {
        out.push(c);
    }
    out.push('…');
    out
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
    dependencies: Vec<CargoDependency>,
}

#[derive(Debug, Deserialize)]
struct CargoDependency {
    name: String,
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
