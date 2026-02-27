//! Dependency maintenance audit for Rust projects.

mod api;
mod model;

use crate::command::za_config;
use anyhow::{Context, Result, anyhow, bail};
use humantime::format_rfc3339_seconds;
use indicatif::{ProgressBar, ProgressStyle};
use reqx::{RetryPolicy, blocking::Client};
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
    print_report(&manifest_path, &summary, &records);

    if let Some(path) = json_out {
        write_json_report(path, &manifest_path, &summary, &records)?;
    }

    let _ = api.flush_cache();

    if fail_on_high && summary.high > 0 {
        bail!("dependency audit found {} high-risk entries", summary.high);
    }
    Ok(())
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
        .arg("--no-deps")
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

    for package_id in package_ids {
        let package = package_by_id
            .get(package_id)
            .ok_or_else(|| anyhow!("workspace package id not found in metadata: {package_id}"))?;
        for dep in &package.dependencies {
            if dep.optional && !include_optional {
                continue;
            }

            let kind = dep.kind.as_deref().unwrap_or("normal");
            if kind == "dev" && !include_dev {
                continue;
            }
            if kind == "build" && !include_build {
                continue;
            }
            if kind != "normal" && kind != "dev" && kind != "build" {
                continue;
            }

            let entry = collected.entry(dep.name.clone()).or_default();
            entry.requirements.insert(dep.req.clone());
            entry.kinds.insert(kind.to_string());
            entry.optional = entry.optional && dep.optional;
        }
    }

    let mut out = Vec::with_capacity(collected.len());
    for (name, builder) in collected {
        out.push(DependencySpec {
            name,
            requirement: join_set(&builder.requirements),
            kinds: join_set(&builder.kinds),
            optional: builder.optional,
        });
    }
    Ok(out)
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

fn print_report(manifest_path: &Path, summary: &AuditSummary, records: &[DepAuditRecord]) {
    println!("Dependency Maintenance Audit");
    println!("Manifest: {}", manifest_path.display());
    println!(
        "Summary: high={} medium={} low={} unknown={}",
        summary.high, summary.medium, summary.low, summary.unknown
    );
    println!(
        "{:<18} {:<15} {:<8} {:<8} {:<10} {:<10} {:<9} NOTES",
        "NAME", "REQ", "RISK", "STARS", "REL_AGE_D", "PUSH_AGE_D", "ARCHIVED"
    );
    for rec in records {
        let stars = rec
            .github_stars
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".to_string());
        let release_age = rec
            .latest_release_age_days
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".to_string());
        let push_age = rec
            .github_push_age_days
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".to_string());
        let archived = rec
            .github_archived
            .map(|v| if v { "yes" } else { "no" }.to_string())
            .unwrap_or_else(|| "-".to_string());
        let notes = rec.notes.join("; ");
        println!(
            "{:<18} {:<15} {:<8} {:<8} {:<10} {:<10} {:<9} {}",
            rec.name,
            truncate(&rec.requirement, 15),
            rec.risk.as_str(),
            stars,
            release_age,
            push_age,
            archived,
            truncate(&notes, 120)
        );
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
}

#[derive(Debug, Deserialize)]
struct CargoPackage {
    id: String,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CrateSnapshot {
    max_version: String,
    updated_at: Option<String>,
    latest_release_at: Option<String>,
    repository: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GitHubRepoResponse {
    stargazers_count: u64,
    archived: bool,
    pushed_at: Option<String>,
}

#[cfg(test)]
mod tests;
