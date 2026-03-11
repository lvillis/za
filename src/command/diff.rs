use anyhow::{Context, Result, anyhow, bail};
use crossterm::terminal;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    env,
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    process::{Command, Output},
};

const DIFF_STAT_BLOCK_COUNT: usize = 5;
const DIFF_STAT_FILLED_BLOCK: &str = "\u{25A0}";
const DIFF_STAT_EMPTY_BLOCK: &str = "\u{25A1}";
const PATH_COLUMN_MIN_WIDTH: usize = 24;
const PATH_COLUMN_MAX_WIDTH: usize = 72;
const LARGE_DIFF_THRESHOLD_FALLBACK: u64 = 400;
const LARGE_DIFF_THRESHOLD_MIN: u64 = 200;
const LARGE_DIFF_THRESHOLD_MAX: u64 = 1200;
const LARGE_DIFF_HISTORY_COMMITS: usize = 200;
const LARGE_DIFF_HISTORY_MIN_SAMPLES: usize = 32;
const LARGE_DIFF_HISTORY_PERCENTILE: usize = 90;
const GENERATED_MARKERS: &[&str] = &[
    "/dist/",
    "/build/",
    "/generated/",
    "/vendor/",
    ".generated.",
    ".min.",
];
const LOCKFILE_NAMES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "poetry.lock",
    "uv.lock",
    "go.sum",
    "composer.lock",
];
const CONFIG_NAMES: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "justfile",
    ".justfile",
    "Dockerfile",
    "docker-compose.yml",
    "docker-compose.yaml",
    "compose.yml",
    "compose.yaml",
    ".env",
    ".env.example",
    ".gitignore",
    ".dockerignore",
];

#[derive(Debug, Clone, Default)]
pub struct DiffRunOptions {
    pub json: bool,
    pub files: bool,
    pub name_only: bool,
    pub path_patterns: Vec<String>,
    pub scopes: Vec<DiffScope>,
    pub exclude_risks: Vec<DiffRiskKind>,
}

pub fn run(options: DiffRunOptions) -> Result<i32> {
    let repo_root = resolve_repo_root()?;
    let filters = DiffFilterSpec::from_run_options(&options, &repo_root)?;
    let report = collect_workspace_diff(&repo_root, options.files || !options.json, &filters)?;

    if options.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).context("serialize diff output")?
        );
    } else {
        print!(
            "{}",
            render_diff_report(
                &report,
                RenderOptions {
                    use_color: color_enabled(),
                    use_unicode_stat: unicode_diff_stat_enabled(),
                    name_only: options.name_only,
                    terminal_width: terminal_width(),
                    interactive: io::stdout().is_terminal(),
                },
            )
        );
    }

    Ok(0)
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum DiffScope {
    Staged,
    Unstaged,
    Untracked,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum DiffRiskKind {
    Binary,
    Ci,
    Config,
    Generated,
    Large,
    Lockfile,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
enum DiffRiskLevel {
    High,
    Medium,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct DiffRisk {
    kind: DiffRiskKind,
    level: DiffRiskLevel,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord, Default)]
#[serde(rename_all = "snake_case")]
enum DiffStatus {
    Added,
    Deleted,
    Renamed,
    Modified,
    Copied,
    TypeChanged,
    Unmerged,
    Untracked,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Default, PartialEq, Eq)]
struct DiffFileStat {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    renamed_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    renamed_to: Option<String>,
    additions: u64,
    deletions: u64,
    binary: bool,
    status: DiffStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    primary_scope: Option<DiffScope>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    scopes: Vec<DiffScope>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    risks: Vec<DiffRisk>,
}

#[derive(Debug, Clone, Serialize, Default, PartialEq, Eq)]
struct DiffSection {
    files: usize,
    additions: u64,
    deletions: u64,
    binary_files: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    file_stats: Vec<DiffFileStat>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct DiffWorkspaceOutput {
    schema_version: u8,
    repo_root: String,
    head: Option<String>,
    clean: bool,
    filters: DiffFilterSummary,
    risk_policy: DiffRiskPolicy,
    workspace_total: DiffSection,
    staged: DiffSection,
    unstaged: DiffSection,
    untracked: DiffSection,
    total: DiffSection,
}

#[derive(Debug, Clone, Serialize, Default, PartialEq, Eq)]
struct DiffFilterSummary {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    scopes: Vec<DiffScope>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    path_patterns: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    exclude_risks: Vec<DiffRiskKind>,
}

#[derive(Debug, Clone)]
struct DiffFilterSpec {
    summary: DiffFilterSummary,
    path_matcher: Option<Gitignore>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct DiffRiskPolicy {
    large_threshold: u64,
    large_threshold_source: DiffLargeThresholdSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    large_threshold_history_samples: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    large_threshold_history_commits: Option<usize>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum DiffLargeThresholdSource {
    FixedFallback,
    HistoryP90,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct HistoricalDiffSamples {
    totals: Vec<u64>,
    commits: usize,
}

#[derive(Clone, Copy)]
struct RenderOptions {
    use_color: bool,
    use_unicode_stat: bool,
    name_only: bool,
    terminal_width: Option<usize>,
    interactive: bool,
}

#[derive(Clone, Copy)]
enum DiffScopeLabelMode {
    Full,
    Compact,
}

#[derive(Clone, Copy)]
struct DiffTableLayout {
    path_width: usize,
    scope_width: usize,
    show_attention: bool,
    show_stat: bool,
    scope_mode: DiffScopeLabelMode,
}

#[derive(Clone, Copy)]
enum NumstatPathMode {
    Native,
    NoIndex,
}

#[derive(Debug, Clone)]
struct RawNumstatEntry {
    path: String,
    previous_path: Option<String>,
    additions: u64,
    deletions: u64,
    binary: bool,
}

#[derive(Debug, Clone)]
struct RawStatusEntry {
    path: String,
    previous_path: Option<String>,
    status: DiffStatus,
}

fn resolve_repo_root() -> Result<PathBuf> {
    let cwd = env::current_dir().context("read current working directory")?;
    resolve_repo_root_from(&cwd)
}

fn resolve_repo_root_from(cwd: &Path) -> Result<PathBuf> {
    let output = git_output(cwd, &["rev-parse", "--show-toplevel"])?;
    if output.status.success() {
        let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if root.is_empty() {
            bail!("`git rev-parse --show-toplevel` returned an empty repository root");
        }
        return Ok(PathBuf::from(root));
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if is_not_git_repository(&stderr) {
        bail!(
            "current directory is not inside a Git repository; `za diff` only works in a Git workspace"
        );
    }
    bail!("`git rev-parse --show-toplevel` failed: {}", stderr.trim())
}

fn collect_workspace_diff(
    repo_root: &Path,
    include_files: bool,
    filters: &DiffFilterSpec,
) -> Result<DiffWorkspaceOutput> {
    let head = git_head_short(repo_root)?;
    let risk_policy = detect_risk_policy(repo_root)?;
    let raw_staged_entries = collect_git_diff_entries(
        repo_root,
        &["diff", "--cached", "--numstat", "-z", "-M", "--root", "--"],
        &[
            "diff",
            "--cached",
            "--name-status",
            "-z",
            "-M",
            "--root",
            "--",
        ],
        NumstatPathMode::Native,
        DiffScope::Staged,
    )?;
    let raw_unstaged_entries = collect_git_diff_entries(
        repo_root,
        &["diff", "--numstat", "-z", "-M", "--"],
        &["diff", "--name-status", "-z", "-M", "--"],
        NumstatPathMode::Native,
        DiffScope::Unstaged,
    )?;
    let raw_untracked_entries = collect_untracked_entries(repo_root)?;
    let mut raw_staged_entries = raw_staged_entries;
    let mut raw_unstaged_entries = raw_unstaged_entries;
    let mut raw_untracked_entries = raw_untracked_entries;
    finalize_entries(&mut raw_staged_entries, &risk_policy);
    finalize_entries(&mut raw_unstaged_entries, &risk_policy);
    finalize_entries(&mut raw_untracked_entries, &risk_policy);
    let workspace_total = build_total_section(
        [
            &raw_staged_entries,
            &raw_unstaged_entries,
            &raw_untracked_entries,
        ],
        false,
        &risk_policy,
    );
    let clean = workspace_total.files == 0;

    let staged_entries = apply_filters(raw_staged_entries, filters);
    let unstaged_entries = apply_filters(raw_unstaged_entries, filters);
    let untracked_entries = apply_filters(raw_untracked_entries, filters);

    let staged = build_diff_section(staged_entries.clone(), include_files, &risk_policy);
    let unstaged = build_diff_section(unstaged_entries.clone(), include_files, &risk_policy);
    let untracked = build_diff_section(untracked_entries.clone(), include_files, &risk_policy);
    let total = build_total_section(
        [&staged_entries, &unstaged_entries, &untracked_entries],
        include_files,
        &risk_policy,
    );

    Ok(DiffWorkspaceOutput {
        schema_version: 1,
        repo_root: repo_root.display().to_string(),
        head,
        clean,
        filters: filters.summary.clone(),
        risk_policy,
        workspace_total,
        staged,
        unstaged,
        untracked,
        total,
    })
}

fn build_diff_section(
    mut entries: Vec<DiffFileStat>,
    include_files: bool,
    risk_policy: &DiffRiskPolicy,
) -> DiffSection {
    finalize_entries(&mut entries, risk_policy);
    sort_section_entries(&mut entries);
    let additions = entries.iter().map(|entry| entry.additions).sum();
    let deletions = entries.iter().map(|entry| entry.deletions).sum();
    let binary_files = entries.iter().filter(|entry| entry.binary).count();
    let files = entries.len();
    if !include_files {
        entries.clear();
    }
    DiffSection {
        files,
        additions,
        deletions,
        binary_files,
        file_stats: entries,
    }
}

fn build_total_section(
    sections: [&Vec<DiffFileStat>; 3],
    include_files: bool,
    risk_policy: &DiffRiskPolicy,
) -> DiffSection {
    let mut merged = BTreeMap::<String, DiffFileStat>::new();
    for section in sections {
        for entry in section {
            let aggregate = merged
                .entry(entry.path.clone())
                .or_insert_with(|| DiffFileStat {
                    path: entry.path.clone(),
                    previous_path: entry.previous_path.clone(),
                    renamed_from: None,
                    renamed_to: None,
                    additions: 0,
                    deletions: 0,
                    binary: false,
                    status: entry.status,
                    primary_scope: None,
                    scopes: Vec::new(),
                    risks: Vec::new(),
                });
            aggregate.previous_path = aggregate
                .previous_path
                .clone()
                .or_else(|| entry.previous_path.clone());
            aggregate.additions = aggregate.additions.saturating_add(entry.additions);
            aggregate.deletions = aggregate.deletions.saturating_add(entry.deletions);
            aggregate.binary |= entry.binary;
            aggregate.status = merge_status(aggregate.status, entry.status);
            for scope in &entry.scopes {
                if !aggregate.scopes.contains(scope) {
                    aggregate.scopes.push(*scope);
                }
            }
        }
    }

    let mut entries = merged.into_values().collect::<Vec<_>>();
    finalize_entries(&mut entries, risk_policy);
    sort_review_entries(&mut entries);
    let additions = entries.iter().map(|entry| entry.additions).sum();
    let deletions = entries.iter().map(|entry| entry.deletions).sum();
    let binary_files = entries.iter().filter(|entry| entry.binary).count();
    let files = entries.len();
    if !include_files {
        entries.clear();
    }
    DiffSection {
        files,
        additions,
        deletions,
        binary_files,
        file_stats: entries,
    }
}

impl DiffFilterSpec {
    fn from_run_options(options: &DiffRunOptions, repo_root: &Path) -> Result<Self> {
        let summary = DiffFilterSummary {
            scopes: normalize_scope_filters(&options.scopes),
            path_patterns: options.path_patterns.clone(),
            exclude_risks: normalize_risk_filters(&options.exclude_risks),
        };
        let path_matcher = build_path_matcher(repo_root, &summary.path_patterns)?;
        Ok(Self {
            summary,
            path_matcher,
        })
    }
}

impl DiffRiskPolicy {
    fn fallback() -> Self {
        Self {
            large_threshold: LARGE_DIFF_THRESHOLD_FALLBACK,
            large_threshold_source: DiffLargeThresholdSource::FixedFallback,
            large_threshold_history_samples: None,
            large_threshold_history_commits: None,
        }
    }
}

fn detect_risk_policy(repo_root: &Path) -> Result<DiffRiskPolicy> {
    let Some(history) = collect_historical_diff_samples(repo_root)? else {
        return Ok(DiffRiskPolicy::fallback());
    };
    if history.totals.len() < LARGE_DIFF_HISTORY_MIN_SAMPLES {
        return Ok(DiffRiskPolicy::fallback());
    }

    let large_threshold = compute_large_diff_threshold(&history.totals);
    Ok(DiffRiskPolicy {
        large_threshold,
        large_threshold_source: DiffLargeThresholdSource::HistoryP90,
        large_threshold_history_samples: Some(history.totals.len()),
        large_threshold_history_commits: Some(history.commits),
    })
}

fn collect_historical_diff_samples(repo_root: &Path) -> Result<Option<HistoricalDiffSamples>> {
    let commit_limit = LARGE_DIFF_HISTORY_COMMITS.to_string();
    let output = git_output(
        repo_root,
        &[
            "log",
            "--no-merges",
            "--no-renames",
            "--numstat",
            "-z",
            "--format=%x1e",
            "-n",
            &commit_limit,
            "--",
        ],
    )?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if is_unborn_head(&stderr) || stderr.contains("does not have any commits yet") {
            return Ok(None);
        }
        return Ok(None);
    }

    let history = parse_historical_diff_samples(&output.stdout)?;
    if history.commits == 0 {
        Ok(None)
    } else {
        Ok(Some(history))
    }
}

fn parse_historical_diff_samples(raw: &[u8]) -> Result<HistoricalDiffSamples> {
    let mut history = HistoricalDiffSamples::default();
    for token in raw.split(|byte| *byte == b'\0') {
        if token.is_empty() {
            continue;
        }

        let mut token = String::from_utf8_lossy(token).into_owned();
        let commit_markers = token.matches('\u{1e}').count();
        if commit_markers > 0 {
            history.commits += commit_markers;
            token.retain(|ch| ch != '\u{1e}');
        }

        let token = token.trim_matches(|ch| matches!(ch, '\n' | '\r'));
        if token.is_empty() {
            continue;
        }

        let mut fields = token.splitn(3, '\t');
        let Some(additions) = fields.next() else {
            continue;
        };
        let Some(deletions) = fields.next() else {
            continue;
        };
        let Some(path) = fields.next() else {
            continue;
        };
        if path.trim().is_empty() || additions == "-" || deletions == "-" {
            continue;
        }

        let additions = additions
            .parse::<u64>()
            .with_context(|| format!("parse historical git additions from `{additions}`"))?;
        let deletions = deletions
            .parse::<u64>()
            .with_context(|| format!("parse historical git deletions from `{deletions}`"))?;
        let total = additions.saturating_add(deletions);
        if total > 0 {
            history.totals.push(total);
        }
    }
    Ok(history)
}

fn compute_large_diff_threshold(samples: &[u64]) -> u64 {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let percentile_index = ((sorted.len() * LARGE_DIFF_HISTORY_PERCENTILE).saturating_sub(1)) / 100;
    sorted[percentile_index].clamp(LARGE_DIFF_THRESHOLD_MIN, LARGE_DIFF_THRESHOLD_MAX)
}

fn build_path_matcher(repo_root: &Path, patterns: &[String]) -> Result<Option<Gitignore>> {
    if patterns.is_empty() {
        return Ok(None);
    }

    let mut builder = GitignoreBuilder::new(repo_root);
    for pattern in patterns {
        builder
            .add_line(None, pattern)
            .with_context(|| format!("invalid `za diff --path` pattern `{pattern}`"))?;
    }

    Ok(Some(
        builder
            .build()
            .context("compile `za diff --path` matchers")?,
    ))
}

fn normalize_scope_filters(scopes: &[DiffScope]) -> Vec<DiffScope> {
    let mut normalized = scopes.to_vec();
    normalized.sort();
    normalized.dedup();
    normalized
}

fn normalize_risk_filters(risks: &[DiffRiskKind]) -> Vec<DiffRiskKind> {
    let mut normalized = risks.to_vec();
    normalized.sort();
    normalized.dedup();
    normalized
}

fn apply_filters(entries: Vec<DiffFileStat>, filters: &DiffFilterSpec) -> Vec<DiffFileStat> {
    entries
        .into_iter()
        .filter(|entry| entry_matches_filters(entry, filters))
        .collect()
}

fn entry_matches_filters(entry: &DiffFileStat, filters: &DiffFilterSpec) -> bool {
    if !filters.summary.scopes.is_empty()
        && !entry
            .scopes
            .iter()
            .any(|scope| filters.summary.scopes.contains(scope))
    {
        return false;
    }

    if let Some(path_matcher) = &filters.path_matcher {
        let relative_path = Path::new(&entry.path);
        if !path_matcher.matched(relative_path, false).is_ignore()
            && !entry.previous_path.as_ref().is_some_and(|previous_path| {
                path_matcher
                    .matched(Path::new(previous_path), false)
                    .is_ignore()
            })
        {
            return false;
        }
    }

    if !filters.summary.exclude_risks.is_empty()
        && entry
            .risks
            .iter()
            .any(|risk| filters.summary.exclude_risks.contains(&risk.kind))
    {
        return false;
    }

    true
}

fn finalize_entries(entries: &mut [DiffFileStat], risk_policy: &DiffRiskPolicy) {
    for entry in entries {
        entry.primary_scope = primary_scope_for(&entry.scopes);
        entry.renamed_from = matches!(entry.status, DiffStatus::Renamed | DiffStatus::Copied)
            .then(|| entry.previous_path.clone())
            .flatten();
        entry.renamed_to = entry.renamed_from.as_ref().map(|_| entry.path.clone());
        entry.risks = detect_risks(entry, risk_policy);
    }
}

fn primary_scope_for(scopes: &[DiffScope]) -> Option<DiffScope> {
    if scopes.contains(&DiffScope::Unstaged) {
        Some(DiffScope::Unstaged)
    } else if scopes.contains(&DiffScope::Staged) {
        Some(DiffScope::Staged)
    } else if scopes.contains(&DiffScope::Untracked) {
        Some(DiffScope::Untracked)
    } else {
        None
    }
}

fn collect_git_diff_entries(
    repo_root: &Path,
    numstat_args: &[&str],
    status_args: &[&str],
    path_mode: NumstatPathMode,
    scope: DiffScope,
) -> Result<Vec<DiffFileStat>> {
    let numstat_output = git_output(repo_root, numstat_args)?;
    if !numstat_output.status.success() {
        let stderr = String::from_utf8_lossy(&numstat_output.stderr);
        bail!("`git {}` failed: {}", numstat_args.join(" "), stderr.trim());
    }

    let status_output = git_output(repo_root, status_args)?;
    if !status_output.status.success() {
        let stderr = String::from_utf8_lossy(&status_output.stderr);
        bail!("`git {}` failed: {}", status_args.join(" "), stderr.trim());
    }

    let numstats = parse_numstat_z(&numstat_output.stdout, path_mode)?;
    let statuses = parse_name_status_z(&status_output.stdout)?;
    merge_diff_entries(numstats, statuses, scope)
}

fn collect_untracked_entries(repo_root: &Path) -> Result<Vec<DiffFileStat>> {
    let output = git_output(
        repo_root,
        &["ls-files", "-z", "--others", "--exclude-standard", "--"],
    )?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "`git ls-files -z --others --exclude-standard --` failed: {}",
            stderr.trim()
        );
    }

    let mut entries = Vec::new();
    for path in parse_nul_paths(&output.stdout)? {
        let numstat_output = git_output_allow_codes(
            repo_root,
            &[
                "diff",
                "--no-index",
                "--numstat",
                "-z",
                "--",
                "/dev/null",
                &path,
            ],
            &[1],
        )?;
        let numstats = parse_numstat_z(&numstat_output.stdout, NumstatPathMode::NoIndex)?;
        for entry in numstats {
            entries.push(DiffFileStat {
                path: entry.path,
                previous_path: None,
                renamed_from: None,
                renamed_to: None,
                additions: entry.additions,
                deletions: entry.deletions,
                binary: entry.binary,
                status: DiffStatus::Untracked,
                primary_scope: Some(DiffScope::Untracked),
                scopes: vec![DiffScope::Untracked],
                risks: Vec::new(),
            });
        }
    }

    Ok(entries)
}

fn parse_numstat_z(raw: &[u8], path_mode: NumstatPathMode) -> Result<Vec<RawNumstatEntry>> {
    let mut index = 0;
    let mut entries = Vec::new();
    while index < raw.len() {
        let additions = take_until(raw, &mut index, b'\t')?;
        let deletions = take_until(raw, &mut index, b'\t')?;

        let (previous_path, path) = if index < raw.len() && raw[index] == b'\0' {
            index += 1;
            let previous_path = take_until(raw, &mut index, b'\0')?;
            let path = take_until(raw, &mut index, b'\0')?;
            (
                normalize_previous_path(previous_path, path_mode),
                normalize_current_path(path, path_mode)?,
            )
        } else {
            (
                None,
                normalize_current_path(take_until(raw, &mut index, b'\0')?, path_mode)?,
            )
        };

        let binary = additions == "-" || deletions == "-";
        let additions = if binary {
            0
        } else {
            additions
                .parse::<u64>()
                .with_context(|| format!("parse git numstat additions from `{additions}`"))?
        };
        let deletions = if binary {
            0
        } else {
            deletions
                .parse::<u64>()
                .with_context(|| format!("parse git numstat deletions from `{deletions}`"))?
        };

        entries.push(RawNumstatEntry {
            path,
            previous_path,
            additions,
            deletions,
            binary,
        });
    }
    Ok(entries)
}

fn parse_name_status_z(raw: &[u8]) -> Result<Vec<RawStatusEntry>> {
    let mut index = 0;
    let mut entries = Vec::new();
    while index < raw.len() {
        let code = take_until(raw, &mut index, b'\0')?;
        let status = parse_diff_status(&code);
        let (previous_path, path) = if matches!(status, DiffStatus::Renamed | DiffStatus::Copied) {
            (
                Some(take_until(raw, &mut index, b'\0')?),
                take_until(raw, &mut index, b'\0')?,
            )
        } else {
            (None, take_until(raw, &mut index, b'\0')?)
        };

        entries.push(RawStatusEntry {
            path,
            previous_path,
            status,
        });
    }
    Ok(entries)
}

fn merge_diff_entries(
    numstats: Vec<RawNumstatEntry>,
    statuses: Vec<RawStatusEntry>,
    scope: DiffScope,
) -> Result<Vec<DiffFileStat>> {
    let mut merged = BTreeMap::<String, DiffFileStat>::new();

    for entry in numstats {
        merged.insert(
            entry.path.clone(),
            DiffFileStat {
                path: entry.path,
                previous_path: entry.previous_path,
                renamed_from: None,
                renamed_to: None,
                additions: entry.additions,
                deletions: entry.deletions,
                binary: entry.binary,
                status: DiffStatus::Unknown,
                primary_scope: Some(scope),
                scopes: vec![scope],
                risks: Vec::new(),
            },
        );
    }

    for entry in statuses {
        let file = merged
            .entry(entry.path.clone())
            .or_insert_with(|| DiffFileStat {
                path: entry.path.clone(),
                previous_path: entry.previous_path.clone(),
                renamed_from: None,
                renamed_to: None,
                additions: 0,
                deletions: 0,
                binary: false,
                status: entry.status,
                primary_scope: Some(scope),
                scopes: vec![scope],
                risks: Vec::new(),
            });
        file.status = entry.status;
        file.previous_path = file.previous_path.clone().or(entry.previous_path);
        file.primary_scope = Some(primary_scope_for(&file.scopes).unwrap_or(scope));
        if !file.scopes.contains(&scope) {
            file.scopes.push(scope);
        }
    }

    let entries = merged.into_values().collect::<Vec<_>>();
    for entry in &entries {
        if entry.path.trim().is_empty() {
            return Err(anyhow!("invalid git diff entry: empty path"));
        }
    }
    Ok(entries)
}

fn take_until(raw: &[u8], index: &mut usize, delimiter: u8) -> Result<String> {
    let start = *index;
    while *index < raw.len() && raw[*index] != delimiter {
        *index += 1;
    }
    if *index >= raw.len() {
        bail!("invalid git -z output: missing field delimiter");
    }
    let value = String::from_utf8_lossy(&raw[start..*index]).to_string();
    *index += 1;
    Ok(value)
}

fn parse_nul_paths(raw: &[u8]) -> Result<Vec<String>> {
    let mut index = 0;
    let mut paths = Vec::new();
    while index < raw.len() {
        let path = take_until(raw, &mut index, b'\0')?;
        if !path.is_empty() {
            paths.push(path);
        }
    }
    Ok(paths)
}

fn normalize_current_path(path: String, path_mode: NumstatPathMode) -> Result<String> {
    let trimmed = path.trim().to_string();
    if trimmed.is_empty() {
        bail!("invalid git numstat entry: empty path");
    }
    match path_mode {
        NumstatPathMode::Native => Ok(trimmed),
        NumstatPathMode::NoIndex => {
            if trimmed == "/dev/null" {
                bail!("invalid git no-index numstat entry: current path cannot be /dev/null");
            }
            Ok(trimmed)
        }
    }
}

fn normalize_previous_path(path: String, path_mode: NumstatPathMode) -> Option<String> {
    let trimmed = path.trim().to_string();
    if trimmed.is_empty() {
        return None;
    }
    match path_mode {
        NumstatPathMode::Native => Some(trimmed),
        NumstatPathMode::NoIndex => (trimmed != "/dev/null").then_some(trimmed),
    }
}

fn parse_diff_status(code: &str) -> DiffStatus {
    match code.chars().next().unwrap_or('?') {
        'A' => DiffStatus::Added,
        'D' => DiffStatus::Deleted,
        'R' => DiffStatus::Renamed,
        'M' => DiffStatus::Modified,
        'C' => DiffStatus::Copied,
        'T' => DiffStatus::TypeChanged,
        'U' => DiffStatus::Unmerged,
        _ => DiffStatus::Unknown,
    }
}

impl From<crate::cli::DiffRiskFilter> for DiffRiskKind {
    fn from(value: crate::cli::DiffRiskFilter) -> Self {
        match value {
            crate::cli::DiffRiskFilter::Binary => Self::Binary,
            crate::cli::DiffRiskFilter::Ci => Self::Ci,
            crate::cli::DiffRiskFilter::Config => Self::Config,
            crate::cli::DiffRiskFilter::Generated => Self::Generated,
            crate::cli::DiffRiskFilter::Large => Self::Large,
            crate::cli::DiffRiskFilter::Lockfile => Self::Lockfile,
        }
    }
}

fn merge_status(lhs: DiffStatus, rhs: DiffStatus) -> DiffStatus {
    if diff_status_rank(rhs) < diff_status_rank(lhs) {
        rhs
    } else {
        lhs
    }
}

fn detect_risks(entry: &DiffFileStat, risk_policy: &DiffRiskPolicy) -> Vec<DiffRisk> {
    let mut risks = Vec::new();
    let normalized_path = entry.path.to_ascii_lowercase();
    let path_components = Path::new(&entry.path)
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .map(|component| component.to_ascii_lowercase())
        .collect::<Vec<_>>();
    let file_name = Path::new(&entry.path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&entry.path);
    let file_name_lower = file_name.to_ascii_lowercase();
    let total = review_impact(entry);

    if entry.binary {
        risks.push(DiffRisk {
            kind: DiffRiskKind::Binary,
            level: DiffRiskLevel::High,
        });
    }
    if normalized_path.starts_with(".github/workflows/")
        || normalized_path.contains("/.github/workflows/")
        || normalized_path.ends_with(".github/workflows")
    {
        risks.push(DiffRisk {
            kind: DiffRiskKind::Ci,
            level: DiffRiskLevel::High,
        });
    }
    if LOCKFILE_NAMES
        .iter()
        .any(|name| file_name_lower == name.to_ascii_lowercase())
    {
        risks.push(DiffRisk {
            kind: DiffRiskKind::Lockfile,
            level: DiffRiskLevel::Medium,
        });
    }
    if CONFIG_NAMES.iter().any(|name| {
        let lower = name.to_ascii_lowercase();
        file_name_lower == lower || normalized_path.ends_with(&format!("/{lower}"))
    }) {
        risks.push(DiffRisk {
            kind: DiffRiskKind::Config,
            level: if entry.status == DiffStatus::Deleted {
                DiffRiskLevel::High
            } else {
                DiffRiskLevel::Medium
            },
        });
    }
    if GENERATED_MARKERS
        .iter()
        .any(|marker| normalized_path.contains(marker))
        || path_components.iter().any(|component| {
            matches!(
                component.as_str(),
                "dist" | "build" | "generated" | "vendor"
            )
        })
        || normalized_path.ends_with(".pb.go")
        || normalized_path.ends_with(".generated.rs")
    {
        risks.push(DiffRisk {
            kind: DiffRiskKind::Generated,
            level: DiffRiskLevel::Medium,
        });
    }
    if total >= risk_policy.large_threshold {
        risks.push(DiffRisk {
            kind: DiffRiskKind::Large,
            level: DiffRiskLevel::High,
        });
    }

    risks.sort_by(|a, b| {
        risk_sort_rank(a.kind)
            .cmp(&risk_sort_rank(b.kind))
            .then_with(|| a.kind.cmp(&b.kind))
    });
    risks.dedup_by(|lhs, rhs| lhs.kind == rhs.kind);
    risks
}

fn risk_sort_rank(kind: DiffRiskKind) -> usize {
    match kind {
        DiffRiskKind::Large => 0,
        DiffRiskKind::Config => 1,
        DiffRiskKind::Ci => 2,
        DiffRiskKind::Binary => 3,
        DiffRiskKind::Lockfile => 4,
        DiffRiskKind::Generated => 5,
    }
}

fn risk_level_rank(level: DiffRiskLevel) -> usize {
    match level {
        DiffRiskLevel::High => 0,
        DiffRiskLevel::Medium => 1,
    }
}

fn review_risk_rank(entry: &DiffFileStat) -> usize {
    entry
        .risks
        .iter()
        .map(|risk| risk_level_rank(risk.level) * 10 + risk_sort_rank(risk.kind))
        .min()
        .unwrap_or(usize::MAX)
}

fn diff_status_rank(status: DiffStatus) -> usize {
    match status {
        DiffStatus::Deleted => 0,
        DiffStatus::Renamed => 1,
        DiffStatus::Added => 2,
        DiffStatus::Modified => 3,
        DiffStatus::Copied => 4,
        DiffStatus::TypeChanged => 5,
        DiffStatus::Unmerged => 6,
        DiffStatus::Untracked => 7,
        DiffStatus::Unknown => 8,
    }
}

fn sort_section_entries(entries: &mut [DiffFileStat]) {
    entries.sort_by(|a, b| {
        review_risk_rank(a)
            .cmp(&review_risk_rank(b))
            .then_with(|| review_impact(b).cmp(&review_impact(a)))
            .then_with(|| diff_status_rank(a.status).cmp(&diff_status_rank(b.status)))
            .then_with(|| a.path.cmp(&b.path))
    });
}

fn sort_review_entries(entries: &mut [DiffFileStat]) {
    entries.sort_by(|a, b| {
        review_risk_rank(a)
            .cmp(&review_risk_rank(b))
            .then_with(|| review_category_rank(a).cmp(&review_category_rank(b)))
            .then_with(|| review_impact(b).cmp(&review_impact(a)))
            .then_with(|| review_scope_rank(a).cmp(&review_scope_rank(b)))
            .then_with(|| diff_status_rank(a.status).cmp(&diff_status_rank(b.status)))
            .then_with(|| a.path.cmp(&b.path))
    });
}

fn review_scope_rank(entry: &DiffFileStat) -> usize {
    let has_unstaged = entry.scopes.contains(&DiffScope::Unstaged);
    let has_staged = entry.scopes.contains(&DiffScope::Staged);
    let has_untracked = entry.scopes.contains(&DiffScope::Untracked);
    match (has_unstaged, has_untracked, has_staged) {
        (true, _, true) => 0,
        (true, _, false) => 1,
        (false, true, _) => 2,
        (false, false, true) => 3,
        _ => 4,
    }
}

fn review_impact(entry: &DiffFileStat) -> u64 {
    let diff = entry.additions.saturating_add(entry.deletions);
    if entry.binary { diff.max(1) } else { diff }
}

fn review_category_rank(entry: &DiffFileStat) -> usize {
    let path = entry.path.to_ascii_lowercase();
    if is_config_like_path(&path) || is_ci_like_path(&path) {
        return 0;
    }
    if is_source_like_path(&path) {
        return 1;
    }
    if is_test_like_path(&path) {
        return 2;
    }
    if is_doc_like_path(&path) {
        return 3;
    }
    if entry.risks.iter().any(|risk| {
        matches!(
            risk.kind,
            DiffRiskKind::Binary | DiffRiskKind::Generated | DiffRiskKind::Lockfile
        )
    }) {
        return 5;
    }
    4
}

fn is_ci_like_path(path: &str) -> bool {
    path.starts_with(".github/workflows/")
        || path.starts_with(".gitlab-ci")
        || path.ends_with("/ci.yml")
        || path.ends_with("/ci.yaml")
}

fn is_config_like_path(path: &str) -> bool {
    let file_name = path.rsplit('/').next().unwrap_or(path);
    CONFIG_NAMES
        .iter()
        .any(|name| name.eq_ignore_ascii_case(file_name))
        || path.ends_with(".toml")
        || path.ends_with(".yaml")
        || path.ends_with(".yml")
        || path.ends_with(".json")
        || path.ends_with(".env")
        || path.ends_with(".ini")
        || path.ends_with(".cfg")
        || path.ends_with(".conf")
        || path.ends_with(".service")
        || path.ends_with(".sh")
}

fn is_source_like_path(path: &str) -> bool {
    path.starts_with("src/")
        || path.starts_with("app/")
        || path.starts_with("lib/")
        || [
            ".rs", ".go", ".py", ".js", ".jsx", ".ts", ".tsx", ".java", ".kt", ".swift", ".rb",
            ".php", ".c", ".cc", ".cpp", ".h", ".hpp", ".cs",
        ]
        .iter()
        .any(|ext| path.ends_with(ext))
}

fn is_test_like_path(path: &str) -> bool {
    path.starts_with("tests/")
        || path.contains("/tests/")
        || path.contains("/test/")
        || path.ends_with("_test.rs")
        || path.ends_with(".spec.ts")
        || path.ends_with(".spec.js")
        || path.ends_with(".test.ts")
        || path.ends_with(".test.js")
}

fn is_doc_like_path(path: &str) -> bool {
    path.starts_with("docs/")
        || path.ends_with(".md")
        || path.ends_with(".rst")
        || path.ends_with(".txt")
}

fn git_head_short(repo_root: &Path) -> Result<Option<String>> {
    let output = git_output(repo_root, &["rev-parse", "--verify", "--short", "HEAD"])?;
    if output.status.success() {
        let head = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return Ok((!head.is_empty()).then_some(head));
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if is_unborn_head(&stderr) {
        return Ok(None);
    }
    bail!(
        "`git rev-parse --verify --short HEAD` failed: {}",
        stderr.trim()
    )
}

fn git_output_allow_codes(cwd: &Path, args: &[&str], allowed: &[i32]) -> Result<Output> {
    let output = git_output(cwd, args)?;
    if output.status.success()
        || output
            .status
            .code()
            .is_some_and(|code| allowed.contains(&code))
    {
        return Ok(output);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!("`git {}` failed: {}", args.join(" "), stderr.trim())
}

fn git_output(cwd: &Path, args: &[&str]) -> Result<Output> {
    match Command::new("git").args(args).current_dir(cwd).output() {
        Ok(output) => Ok(output),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            bail!("`za diff` requires `git`; install it first")
        }
        Err(err) => Err(err).with_context(|| format!("run `git {}`", args.join(" "))),
    }
}

fn is_not_git_repository(stderr: &str) -> bool {
    stderr
        .trim()
        .to_ascii_lowercase()
        .contains("not a git repository")
}

fn is_unborn_head(stderr: &str) -> bool {
    let lower = stderr.trim().to_ascii_lowercase();
    lower.contains("needed a single revision")
        || lower.contains("ambiguous argument 'head'")
        || lower.contains("unknown revision or path not in the working tree")
}

fn color_enabled() -> bool {
    io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none()
}

fn terminal_width() -> Option<usize> {
    io::stdout()
        .is_terminal()
        .then(|| terminal::size().ok().map(|(width, _)| width as usize))
        .flatten()
}

fn unicode_diff_stat_enabled() -> bool {
    io::stdout().is_terminal()
        && env::var_os("NO_COLOR").is_none()
        && ["LC_ALL", "LC_CTYPE", "LANG"]
            .into_iter()
            .find_map(env::var_os)
            .and_then(|value| value.into_string().ok())
            .is_some_and(|value| {
                let value = value.to_ascii_lowercase();
                value.contains("utf-8") || value.contains("utf8")
            })
}

fn render_diff_report(report: &DiffWorkspaceOutput, options: RenderOptions) -> String {
    let use_color = options.use_color;
    let repo_name = Path::new(&report.repo_root)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(&report.repo_root);
    let mut lines = Vec::new();
    lines.push(format!(
        "za diff  {}  {} {}",
        style_bold(repo_name, use_color),
        style_dim("@", use_color),
        style_head(report.head.as_deref().unwrap_or("(unborn)"), use_color),
    ));

    let filter_summary = render_filter_summary(&report.filters, use_color);
    if !filter_summary.is_empty() {
        lines.push(filter_summary);
    }

    if report.clean {
        lines.push(format!(
            "{} {}",
            style_dim("status", use_color),
            "working tree clean"
        ));
        return lines.join("\n") + "\n";
    }

    if report.total.files == 0 {
        lines.push(format!(
            "{} {}",
            style_dim("status", use_color),
            "no changes matched current filters"
        ));
        lines.push(render_workspace_summary(report, use_color));
        return lines.join("\n") + "\n";
    }

    lines.push(format!(
        "{} {}  {}  {}{}",
        style_dim("changed", use_color),
        format_args!(
            "{} {}",
            report.total.files,
            pluralize(report.total.files, "file", "files")
        ),
        colorize_additions(format!("+{}", report.total.additions), use_color),
        colorize_deletions(format!("-{}", report.total.deletions), use_color),
        if report.total.binary_files > 0 {
            format!(
                "  {}",
                colorize_binary(
                    format!(
                        "{} {}",
                        report.total.binary_files,
                        pluralize(report.total.binary_files, "binary", "binaries")
                    ),
                    use_color,
                )
            )
        } else {
            String::new()
        }
    ));

    let scope_summary = render_scope_summary(report, use_color);
    if !scope_summary.is_empty() {
        lines.push(scope_summary);
    }
    if report.filters != DiffFilterSummary::default()
        && report.total.files != report.workspace_total.files
    {
        let workspace_file_summary = format!(
            "{} {}",
            report.workspace_total.files,
            pluralize(
                report.workspace_total.files,
                "workspace file",
                "workspace files"
            )
        );
        lines.push(format!(
            "{} {} {} {}",
            style_dim("matching", use_color),
            report.total.files,
            style_dim("of", use_color),
            workspace_file_summary
        ));
    }
    let attention_summary =
        render_attention_summary(&report.total.file_stats, &report.risk_policy, use_color);
    if !attention_summary.is_empty() {
        lines.push(attention_summary);
    }

    let review_entries = &report.total.file_stats;
    if !review_entries.is_empty() {
        lines.push(String::new());
        let layout = diff_table_layout(review_entries, options);
        if options.name_only {
            render_name_only_table(&mut lines, review_entries, use_color, layout);
        } else {
            render_full_table(&mut lines, review_entries, options, layout);
        }
    }

    lines.join("\n") + "\n"
}

fn render_filter_summary(filters: &DiffFilterSummary, use_color: bool) -> String {
    let mut parts = Vec::new();
    if !filters.scopes.is_empty() {
        parts.push(format!(
            "scope={}",
            filters
                .scopes
                .iter()
                .map(|scope| scope.label())
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    if !filters.path_patterns.is_empty() {
        parts.push(format!("path={}", filters.path_patterns.join(",")));
    }
    if !filters.exclude_risks.is_empty() {
        parts.push(format!(
            "hide={}",
            filters
                .exclude_risks
                .iter()
                .map(|risk| risk.label())
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(
            "{} {}",
            style_dim("filter", use_color),
            style_dim(&parts.join("  "), use_color)
        )
    }
}

fn render_workspace_summary(report: &DiffWorkspaceOutput, use_color: bool) -> String {
    format!(
        "{} {}  {}  {}{}",
        style_dim("workspace", use_color),
        format_args!(
            "{} {}",
            report.workspace_total.files,
            pluralize(report.workspace_total.files, "file", "files")
        ),
        colorize_additions(format!("+{}", report.workspace_total.additions), use_color),
        colorize_deletions(format!("-{}", report.workspace_total.deletions), use_color),
        if report.workspace_total.binary_files > 0 {
            format!(
                "  {}",
                colorize_binary(
                    format!(
                        "{} {}",
                        report.workspace_total.binary_files,
                        pluralize(report.workspace_total.binary_files, "binary", "binaries")
                    ),
                    use_color,
                )
            )
        } else {
            String::new()
        }
    )
}

fn render_attention_summary(
    entries: &[DiffFileStat],
    risk_policy: &DiffRiskPolicy,
    use_color: bool,
) -> String {
    let mut counts = BTreeMap::<DiffRiskKind, usize>::new();
    let mut files_with_risk = 0usize;
    let mut summary_level = DiffRiskLevel::Medium;
    for entry in entries {
        if entry.risks.is_empty() {
            continue;
        }
        files_with_risk += 1;
        for risk in &entry.risks {
            *counts.entry(risk.kind).or_default() += 1;
            if risk.level == DiffRiskLevel::High {
                summary_level = DiffRiskLevel::High;
            }
        }
    }

    if files_with_risk == 0 {
        return String::new();
    }

    let details = counts
        .into_iter()
        .map(|(kind, count)| (risk_sort_rank(kind), count, kind))
        .collect::<Vec<_>>();
    let mut details = details;
    details.sort_by(|lhs, rhs| lhs.0.cmp(&rhs.0).then_with(|| rhs.1.cmp(&lhs.1)));

    format!(
        "{} {} {} {}",
        style_dim("attention", use_color),
        style_risk_summary(
            format!(
                "{} {}",
                files_with_risk,
                pluralize(files_with_risk, "file", "files")
            ),
            summary_level,
            use_color,
        ),
        style_dim("·", use_color),
        details
            .into_iter()
            .take(3)
            .map(|(_, count, kind)| {
                format!("{count} {}", risk_summary_label(kind, risk_policy))
            })
            .collect::<Vec<_>>()
            .join(&format!(" {} ", style_dim("·", use_color))),
    )
}

fn risk_summary_label(kind: DiffRiskKind, risk_policy: &DiffRiskPolicy) -> String {
    match kind {
        DiffRiskKind::Large => format!("large>={}", risk_policy.large_threshold),
        _ => kind.label().to_string(),
    }
}

fn diff_table_layout(entries: &[DiffFileStat], options: RenderOptions) -> DiffTableLayout {
    let show_attention = show_attention_column(entries);
    let scope_mode = match options.terminal_width {
        Some(width) if width < 112 => DiffScopeLabelMode::Compact,
        None if !options.interactive => DiffScopeLabelMode::Compact,
        _ => DiffScopeLabelMode::Full,
    };
    let show_stat =
        options.interactive && !options.name_only && options.terminal_width.unwrap_or(120) >= 96;
    let scope_width = entries
        .iter()
        .map(|entry| review_scope_label(entry, scope_mode))
        .map(|scope| scope.chars().count())
        .max()
        .unwrap_or_default()
        .max(match scope_mode {
            DiffScopeLabelMode::Full => 9,
            DiffScopeLabelMode::Compact => 5,
        });
    let path_width = path_column_width(entries, options, scope_width, show_attention, show_stat);
    DiffTableLayout {
        path_width,
        scope_width,
        show_attention,
        show_stat,
        scope_mode,
    }
}

fn path_column_width(
    entries: &[DiffFileStat],
    options: RenderOptions,
    scope_width: usize,
    show_attention: bool,
    show_stat: bool,
) -> usize {
    let observed = entries
        .iter()
        .map(review_path_plain)
        .map(|path| path.chars().count())
        .max()
        .unwrap_or(PATH_COLUMN_MIN_WIDTH)
        .min(PATH_COLUMN_MAX_WIDTH);
    let Some(terminal_width) = options.terminal_width else {
        return observed;
    };

    let attention_width = if show_attention { 3 } else { 0 };
    let fixed_width = if options.name_only {
        2 + attention_width + scope_width + 2 + 4
    } else if show_stat {
        2 + 2 + attention_width + scope_width + 2 + 8 + 2 + 8 + 2 + DIFF_STAT_BLOCK_COUNT
    } else {
        2 + 2 + attention_width + scope_width + 2 + 8 + 2 + 8
    };
    let available = terminal_width
        .saturating_sub(fixed_width + 4)
        .clamp(PATH_COLUMN_MIN_WIDTH, PATH_COLUMN_MAX_WIDTH);
    observed.min(available)
}

fn show_attention_column(entries: &[DiffFileStat]) -> bool {
    entries.iter().any(|entry| !entry.risks.is_empty())
}

fn render_name_only_table(
    lines: &mut Vec<String>,
    entries: &[DiffFileStat],
    use_color: bool,
    layout: DiffTableLayout,
) {
    lines.push(format!(
        "{}{}  {}  {}{}",
        style_dim("st", use_color),
        render_attention_header(layout.show_attention, use_color),
        style_dim(
            &format!("{:<width$}", "scope", width = layout.scope_width),
            use_color
        ),
        style_dim("file", use_color),
        " ".repeat(layout.path_width.saturating_sub(4)),
    ));

    for entry in entries {
        let status_label = format!("{:>2}", entry.status.short_label());
        let scope_label = review_scope_label(entry, layout.scope_mode);
        let path_plain = review_path_plain_with_width(entry, layout.path_width);
        let path_rendered = review_path_rendered(entry, layout.path_width, use_color);
        let visible_path_width = path_plain.chars().count().min(layout.path_width);
        let path_padding = " ".repeat(layout.path_width.saturating_sub(visible_path_width));
        lines.push(format!(
            "{}{}  {}  {}{}",
            style_status(entry.status, &status_label, use_color),
            render_attention_column(entry, layout.show_attention, use_color),
            style_scope(
                &format!("{scope_label:<width$}", width = layout.scope_width),
                entry,
                use_color
            ),
            path_rendered,
            path_padding,
        ));
    }
}

fn render_full_table(
    lines: &mut Vec<String>,
    entries: &[DiffFileStat],
    options: RenderOptions,
    layout: DiffTableLayout,
) {
    let use_color = options.use_color;
    let max_stat_total = entries
        .iter()
        .map(|entry| entry.additions.saturating_add(entry.deletions))
        .max()
        .unwrap_or_default();
    lines.push(if layout.show_stat {
        format!(
            "{}{}  {}  {}{}  {}  {}  {}",
            style_dim("st", use_color),
            render_attention_header(layout.show_attention, use_color),
            style_dim(
                &format!("{:<width$}", "scope", width = layout.scope_width),
                use_color
            ),
            style_dim("file", use_color),
            " ".repeat(layout.path_width.saturating_sub(4)),
            colorize_additions(format!("{:>8}", "+add"), use_color),
            colorize_deletions(format!("{:>8}", "-del"), use_color),
            style_dim(&format!("{:<DIFF_STAT_BLOCK_COUNT$}", "stat"), use_color),
        )
    } else {
        format!(
            "{}{}  {}  {}{}  {}  {}",
            style_dim("st", use_color),
            render_attention_header(layout.show_attention, use_color),
            style_dim(
                &format!("{:<width$}", "scope", width = layout.scope_width),
                use_color
            ),
            style_dim("file", use_color),
            " ".repeat(layout.path_width.saturating_sub(4)),
            colorize_additions(format!("{:>8}", "+add"), use_color),
            colorize_deletions(format!("{:>8}", "-del"), use_color),
        )
    });

    for entry in entries {
        let status_label = format!("{:>2}", entry.status.short_label());
        let scope_label = review_scope_label(entry, layout.scope_mode);
        let path_plain = review_path_plain_with_width(entry, layout.path_width);
        let path_rendered = review_path_rendered(entry, layout.path_width, use_color);
        let visible_path_width = path_plain.chars().count().min(layout.path_width);
        let path_padding = " ".repeat(layout.path_width.saturating_sub(visible_path_width));
        lines.push(if entry.binary && layout.show_stat {
            format!(
                "{}{}  {}  {}{}  {}  {}",
                style_status(entry.status, &status_label, use_color),
                render_attention_column(entry, layout.show_attention, use_color),
                style_scope(
                    &format!("{scope_label:<width$}", width = layout.scope_width),
                    entry,
                    use_color
                ),
                path_rendered,
                path_padding,
                format_binary_column("binary", use_color),
                render_empty_diff_stat(use_color, options.use_unicode_stat),
            )
        } else if entry.binary {
            format!(
                "{}{}  {}  {}{}  {}  {}",
                style_status(entry.status, &status_label, use_color),
                render_attention_column(entry, layout.show_attention, use_color),
                style_scope(
                    &format!("{scope_label:<width$}", width = layout.scope_width),
                    entry,
                    use_color
                ),
                path_rendered,
                path_padding,
                format_binary_column("binary", use_color),
                style_dim(&format!("{:>8}", "-"), use_color),
            )
        } else {
            let base = format!(
                "{}{}  {}  {}{}  {}  {}",
                style_status(entry.status, &status_label, use_color),
                render_attention_column(entry, layout.show_attention, use_color),
                style_scope(
                    &format!("{scope_label:<width$}", width = layout.scope_width),
                    entry,
                    use_color
                ),
                path_rendered,
                path_padding,
                format_additions_column(&format!("+{}", entry.additions), use_color),
                format_deletions_column(&format!("-{}", entry.deletions), use_color),
            );
            if layout.show_stat {
                format!(
                    "{base}  {}",
                    render_diff_stat_bar(
                        entry,
                        max_stat_total,
                        use_color,
                        options.use_unicode_stat
                    )
                )
            } else {
                base
            }
        });
    }
}

fn render_scope_summary(report: &DiffWorkspaceOutput, use_color: bool) -> String {
    let mut parts = Vec::new();
    if report.unstaged.files > 0 {
        parts.push(style_scope_summary(
            format!(
                "{} {}",
                report.unstaged.files,
                pluralize(report.unstaged.files, "unstaged", "unstaged")
            ),
            DiffScope::Unstaged,
            use_color,
        ));
    }
    if report.staged.files > 0 {
        parts.push(style_scope_summary(
            format!(
                "{} {}",
                report.staged.files,
                pluralize(report.staged.files, "staged", "staged")
            ),
            DiffScope::Staged,
            use_color,
        ));
    }
    if report.untracked.files > 0 {
        parts.push(style_scope_summary(
            format!(
                "{} {}",
                report.untracked.files,
                pluralize(report.untracked.files, "untracked", "untracked")
            ),
            DiffScope::Untracked,
            use_color,
        ));
    }
    parts.join(&format!(" {} ", style_dim("·", use_color)))
}

fn review_scope_label(entry: &DiffFileStat, mode: DiffScopeLabelMode) -> String {
    let mut labels = Vec::new();
    if entry.scopes.contains(&DiffScope::Unstaged) {
        labels.push(match mode {
            DiffScopeLabelMode::Full => "unstaged",
            DiffScopeLabelMode::Compact => "u",
        });
    }
    if entry.scopes.contains(&DiffScope::Staged) {
        labels.push(match mode {
            DiffScopeLabelMode::Full => "staged",
            DiffScopeLabelMode::Compact => "s",
        });
    }
    if entry.scopes.contains(&DiffScope::Untracked) {
        labels.push(match mode {
            DiffScopeLabelMode::Full => "untracked",
            DiffScopeLabelMode::Compact => "?",
        });
    }
    labels.join("+")
}

fn review_path_plain(entry: &DiffFileStat) -> String {
    review_path_plain_with_width(entry, PATH_COLUMN_MAX_WIDTH)
}

fn review_path_plain_with_width(entry: &DiffFileStat, width: usize) -> String {
    if let Some(previous_path) = &entry.previous_path {
        let arrow_width = 4;
        if width <= arrow_width {
            return truncate_path_tail(&entry.path, width);
        }
        let total_width = width.saturating_sub(arrow_width);
        let current_width = ((total_width as f64) * 0.6).round() as usize;
        let current_width = current_width.clamp(1, total_width.saturating_sub(1).max(1));
        let previous_width = total_width.saturating_sub(current_width);
        format!(
            "{} -> {}",
            truncate_path_tail(previous_path, previous_width.max(1)),
            truncate_path_tail(&entry.path, current_width.max(1)),
        )
    } else {
        truncate_path_tail(&entry.path, width)
    }
}

fn review_path_rendered(entry: &DiffFileStat, width: usize, use_color: bool) -> String {
    if let Some(previous_path) = &entry.previous_path {
        let arrow_width = 4;
        if width <= arrow_width {
            return style_path(&truncate_path_tail(&entry.path, width), use_color);
        }
        let total_width = width.saturating_sub(arrow_width);
        let current_width = ((total_width as f64) * 0.6).round() as usize;
        let current_width = current_width.clamp(1, total_width.saturating_sub(1).max(1));
        let previous_width = total_width.saturating_sub(current_width);
        format!(
            "{} {} {}",
            style_path(
                &truncate_path_tail(previous_path, previous_width.max(1)),
                use_color
            ),
            style_dim("->", use_color),
            style_path(
                &truncate_path_tail(&entry.path, current_width.max(1)),
                use_color
            ),
        )
    } else {
        style_path(&truncate_path_tail(&entry.path, width), use_color)
    }
}

fn truncate_path_tail(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_string();
    }
    if width <= 1 {
        return "…".to_string();
    }
    let suffix = value
        .chars()
        .rev()
        .take(width.saturating_sub(1))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    if let Some(offset) = suffix.find('/') {
        return format!("…{}", &suffix[offset..]);
    }
    format!("…{suffix}")
}

fn style_path(path: &str, use_color: bool) -> String {
    if !use_color {
        return path.to_string();
    }

    match path.rsplit_once('/') {
        Some((prefix, file_name)) => format!(
            "{}{}",
            style_dim(&format!("{prefix}/"), use_color),
            style_bold(file_name, use_color),
        ),
        None => style_bold(path, use_color),
    }
}

fn render_attention_header(show_attention: bool, use_color: bool) -> String {
    if !show_attention {
        String::new()
    } else {
        format!("  {}", style_dim("!", use_color))
    }
}

fn render_attention_column(entry: &DiffFileStat, show_attention: bool, use_color: bool) -> String {
    if !show_attention {
        return String::new();
    }

    match highest_risk_level(entry) {
        Some(level) => format!("  {}", style_risk(risk_marker(level), level, use_color)),
        None => format!("  {}", style_dim(" ", use_color)),
    }
}

fn risk_marker(level: DiffRiskLevel) -> &'static str {
    match level {
        DiffRiskLevel::High => "!",
        DiffRiskLevel::Medium => "~",
    }
}

fn style_status(status: DiffStatus, label: &str, use_color: bool) -> String {
    let code = match status {
        DiffStatus::Added => "32",
        DiffStatus::Deleted => "31",
        DiffStatus::Renamed => "36",
        DiffStatus::Modified => "33",
        DiffStatus::Copied => "36",
        DiffStatus::TypeChanged => "35",
        DiffStatus::Unmerged => "31",
        DiffStatus::Untracked => "34",
        DiffStatus::Unknown => "37",
    };
    style_ansi(label, &[code, "1"], use_color)
}

fn style_scope(label: &str, entry: &DiffFileStat, use_color: bool) -> String {
    let code = if entry.scopes.contains(&DiffScope::Unstaged) {
        "33"
    } else if entry.scopes.contains(&DiffScope::Staged) {
        "32"
    } else {
        "34"
    };
    style_ansi(label, &[code, "2"], use_color)
}

fn style_scope_summary(label: String, scope: DiffScope, use_color: bool) -> String {
    let code = match scope {
        DiffScope::Unstaged => "33",
        DiffScope::Staged => "32",
        DiffScope::Untracked => "34",
    };
    style_ansi(&label, &[code], use_color)
}

fn style_head(value: &str, use_color: bool) -> String {
    style_ansi(value, &["2"], use_color)
}

fn style_dim(value: &str, use_color: bool) -> String {
    style_ansi(value, &["2"], use_color)
}

fn style_bold(value: &str, use_color: bool) -> String {
    style_ansi(value, &["1"], use_color)
}

fn style_risk(value: &str, level: DiffRiskLevel, use_color: bool) -> String {
    let codes = match level {
        DiffRiskLevel::High => ["31", "1"],
        DiffRiskLevel::Medium => ["35", "2"],
    };
    style_ansi(value, &codes, use_color)
}

fn style_risk_summary(value: String, level: DiffRiskLevel, use_color: bool) -> String {
    style_risk(&value, level, use_color)
}

fn colorize_additions(value: String, use_color: bool) -> String {
    style_ansi(&value, &["32"], use_color)
}

fn colorize_deletions(value: String, use_color: bool) -> String {
    style_ansi(&value, &["31"], use_color)
}

fn colorize_binary(value: String, use_color: bool) -> String {
    style_ansi(&value, &["36"], use_color)
}

fn format_additions_column(value: &str, use_color: bool) -> String {
    colorize_additions(format!("{value:>8}"), use_color)
}

fn format_deletions_column(value: &str, use_color: bool) -> String {
    colorize_deletions(format!("{value:>8}"), use_color)
}

fn format_binary_column(value: &str, use_color: bool) -> String {
    colorize_binary(format!("{value:>8}"), use_color)
}

fn render_diff_stat_bar(
    entry: &DiffFileStat,
    max_total: u64,
    use_color: bool,
    use_unicode_stat: bool,
) -> String {
    let total = entry.additions.saturating_add(entry.deletions);
    if total == 0 || max_total == 0 {
        return render_empty_diff_stat(use_color, use_unicode_stat);
    }

    let mut bar_width =
        ((total as f64 / max_total as f64) * DIFF_STAT_BLOCK_COUNT as f64).round() as usize;
    if bar_width == 0 {
        bar_width = 1;
    }
    bar_width = bar_width.min(DIFF_STAT_BLOCK_COUNT);

    let mut add_width =
        ((entry.additions as f64 / total as f64) * bar_width as f64).round() as usize;
    add_width = add_width.min(bar_width);
    let mut del_width = bar_width.saturating_sub(add_width);

    if bar_width > 1 && entry.additions > 0 && add_width == 0 {
        add_width = 1;
        del_width = bar_width.saturating_sub(add_width);
    }
    if bar_width > 1 && entry.deletions > 0 && del_width == 0 {
        del_width = 1;
        add_width = bar_width.saturating_sub(del_width);
    }

    let (add_glyph, del_glyph, empty_glyph) = if use_unicode_stat {
        (
            DIFF_STAT_FILLED_BLOCK,
            DIFF_STAT_FILLED_BLOCK,
            DIFF_STAT_EMPTY_BLOCK,
        )
    } else {
        ("+", "-", ".")
    };
    let add_bar = add_glyph.repeat(add_width);
    let del_bar = del_glyph.repeat(del_width);
    let empty_bar = empty_glyph.repeat(DIFF_STAT_BLOCK_COUNT.saturating_sub(bar_width));

    let mut rendered = String::new();
    rendered.push_str(&colorize_additions(add_bar, use_color));
    rendered.push_str(&colorize_deletions(del_bar, use_color));
    rendered.push_str(&style_dim(&empty_bar, use_color));
    rendered
}

fn render_empty_diff_stat(use_color: bool, use_unicode_stat: bool) -> String {
    let glyph = if use_unicode_stat {
        DIFF_STAT_EMPTY_BLOCK
    } else {
        "."
    };
    style_dim(&glyph.repeat(DIFF_STAT_BLOCK_COUNT), use_color)
}

fn style_ansi(value: &str, codes: &[&str], use_color: bool) -> String {
    if !use_color {
        return value.to_string();
    }
    format!("\u{1b}[{}m{}\u{1b}[0m", codes.join(";"), value)
}

fn pluralize<'a>(value: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if value == 1 { singular } else { plural }
}

fn highest_risk_level(entry: &DiffFileStat) -> Option<DiffRiskLevel> {
    entry.risks.iter().map(|risk| risk.level).min()
}

impl DiffScope {
    fn label(self) -> &'static str {
        match self {
            Self::Staged => "staged",
            Self::Unstaged => "unstaged",
            Self::Untracked => "untracked",
        }
    }
}

impl DiffRiskKind {
    fn label(self) -> &'static str {
        match self {
            Self::Binary => "binary",
            Self::Ci => "ci",
            Self::Config => "config",
            Self::Generated => "generated",
            Self::Large => "large",
            Self::Lockfile => "lock",
        }
    }
}

impl DiffStatus {
    fn short_label(self) -> &'static str {
        match self {
            Self::Added => "A",
            Self::Deleted => "D",
            Self::Renamed => "R",
            Self::Modified => "M",
            Self::Copied => "C",
            Self::TypeChanged => "T",
            Self::Unmerged => "U",
            Self::Untracked => "?",
            Self::Unknown => "!",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DIFF_STAT_FILLED_BLOCK, DiffFileStat, DiffFilterSpec, DiffFilterSummary,
        DiffLargeThresholdSource, DiffRisk, DiffRiskKind, DiffRiskLevel, DiffRiskPolicy, DiffScope,
        DiffSection, DiffStatus, DiffWorkspaceOutput, NumstatPathMode, RenderOptions,
        collect_workspace_diff, compute_large_diff_threshold, parse_diff_status,
        parse_historical_diff_samples, parse_name_status_z, parse_numstat_z, render_diff_report,
        resolve_repo_root_from,
    };
    use anyhow::Result;
    use std::{
        fs,
        path::{Path, PathBuf},
        process::Command,
        time::{SystemTime, UNIX_EPOCH},
    };

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(name: &str) -> Result<Self> {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time must be after unix epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "za-diff-test-{name}-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path)?;
            Ok(Self { path })
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn parse_numstat_z_normalizes_no_index_paths() {
        let entries = parse_numstat_z(
            b"2\t0\t\x00/dev/null\x00notes.txt\x00",
            NumstatPathMode::NoIndex,
        )
        .expect("must parse");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "notes.txt");
        assert_eq!(entries[0].previous_path, None);
        assert_eq!(entries[0].additions, 2);
        assert_eq!(entries[0].deletions, 0);
    }

    #[test]
    fn parse_numstat_z_tracks_binary_files() {
        let entries = parse_numstat_z(b"-\t-\tassets/logo.png\x00", NumstatPathMode::Native)
            .expect("must parse");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "assets/logo.png");
        assert!(entries[0].binary);
    }

    #[test]
    fn parse_name_status_z_tracks_renames() {
        let entries =
            parse_name_status_z(b"R100\x00src/old.rs\x00src/new.rs\x00").expect("must parse");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, DiffStatus::Renamed);
        assert_eq!(entries[0].previous_path.as_deref(), Some("src/old.rs"));
        assert_eq!(entries[0].path, "src/new.rs");
    }

    #[test]
    fn parse_diff_status_maps_known_codes() {
        assert_eq!(parse_diff_status("M"), DiffStatus::Modified);
        assert_eq!(parse_diff_status("A"), DiffStatus::Added);
        assert_eq!(parse_diff_status("D"), DiffStatus::Deleted);
        assert_eq!(parse_diff_status("R100"), DiffStatus::Renamed);
        assert_eq!(parse_diff_status("T"), DiffStatus::TypeChanged);
        assert_eq!(parse_diff_status("U"), DiffStatus::Unmerged);
    }

    #[test]
    fn parse_historical_diff_samples_collects_commits_and_totals() {
        let history = parse_historical_diff_samples(
            b"\x1e\x00\n3\t1\tsrc/main.rs\x00\x1e\x00\n5\t0\tREADME.md\x00-\t-\tlogo.png\x00",
        )
        .expect("must parse");
        assert_eq!(history.commits, 2);
        assert_eq!(history.totals, vec![4, 5]);
    }

    #[test]
    fn compute_large_diff_threshold_uses_clamped_p90() {
        let threshold =
            compute_large_diff_threshold(&[5, 8, 13, 21, 34, 55, 89, 144, 233, 377, 610, 987]);
        assert_eq!(threshold, 610);

        let min_threshold = compute_large_diff_threshold(&[1, 2, 3, 4, 5]);
        assert_eq!(min_threshold, 200);

        let max_threshold = compute_large_diff_threshold(&[500, 900, 1300, 2400, 3600]);
        assert_eq!(max_threshold, 1200);
    }

    #[test]
    fn resolve_repo_root_from_errors_outside_git_repo() {
        let dir = TempDir::new("outside").expect("temp dir");
        let err = resolve_repo_root_from(&dir.path).expect_err("must fail");
        assert!(
            err.to_string()
                .contains("current directory is not inside a Git repository")
        );
    }

    #[test]
    fn collect_workspace_diff_counts_staged_unstaged_and_untracked() {
        let dir = TempDir::new("workspace").expect("temp dir");
        init_repo(&dir.path).expect("init repo");
        write_file(dir.path.join("tracked.txt"), "one\n").expect("write tracked");
        git(&dir.path, &["add", "tracked.txt"]).expect("git add");
        git(&dir.path, &["commit", "-qm", "init"]).expect("git commit");

        write_file(dir.path.join("tracked.txt"), "one\ntwo\nthree\n").expect("modify tracked");
        write_file(dir.path.join("staged.txt"), "alpha\nbeta\n").expect("write staged");
        git(&dir.path, &["add", "staged.txt"]).expect("git add staged");
        write_file(dir.path.join("draft.txt"), "note\n").expect("write untracked");

        let report = collect_workspace_diff(Path::new(&dir.path), true, &no_filters())
            .expect("collect diff");
        assert_eq!(report.staged.files, 1);
        assert_eq!(report.staged.additions, 2);
        assert_eq!(report.unstaged.files, 1);
        assert_eq!(report.untracked.files, 1);
        assert_eq!(report.total.files, 3);
        assert_eq!(report.total.additions, 5);
        assert!(!report.clean);
        assert!(
            report
                .total
                .file_stats
                .iter()
                .any(|entry| entry.scopes == vec![DiffScope::Untracked])
        );
    }

    #[test]
    fn collect_workspace_diff_supports_unborn_head() {
        let dir = TempDir::new("unborn").expect("temp dir");
        init_empty_repo(&dir.path).expect("init repo");
        write_file(dir.path.join("new.txt"), "hello\nworld\n").expect("write file");
        git(&dir.path, &["add", "new.txt"]).expect("git add");

        let report = collect_workspace_diff(Path::new(&dir.path), false, &no_filters())
            .expect("collect diff");
        assert_eq!(report.head, None);
        assert_eq!(report.staged.files, 1);
        assert_eq!(report.staged.additions, 2);
        assert_eq!(report.total.files, 1);
    }

    #[test]
    fn collect_workspace_diff_tracks_renamed_files() {
        let dir = TempDir::new("rename").expect("temp dir");
        init_repo(&dir.path).expect("init repo");
        write_file(dir.path.join("src_old.rs"), "fn old() {}\n").expect("write file");
        git(&dir.path, &["add", "src_old.rs"]).expect("git add");
        git(&dir.path, &["commit", "-qm", "init"]).expect("git commit");
        fs::rename(dir.path.join("src_old.rs"), dir.path.join("src_new.rs")).expect("rename file");
        git(&dir.path, &["add", "-A"]).expect("git add rename");

        let report = collect_workspace_diff(Path::new(&dir.path), true, &no_filters())
            .expect("collect diff");
        assert_eq!(report.staged.files, 1);
        assert_eq!(report.staged.file_stats[0].status, DiffStatus::Renamed);
        assert_eq!(
            report.staged.file_stats[0].previous_path.as_deref(),
            Some("src_old.rs")
        );
        assert_eq!(report.staged.file_stats[0].path, "src_new.rs");
    }

    #[test]
    fn render_diff_report_mentions_clean_workspace() {
        let dir = TempDir::new("clean").expect("temp dir");
        init_repo(&dir.path).expect("init repo");
        write_file(dir.path.join("tracked.txt"), "one\n").expect("write tracked");
        git(&dir.path, &["add", "tracked.txt"]).expect("git add");
        git(&dir.path, &["commit", "-qm", "init"]).expect("git commit");

        let report = collect_workspace_diff(Path::new(&dir.path), false, &no_filters())
            .expect("collect diff");
        let rendered = render_diff_report(&report, render_options(false, false, false));
        assert!(rendered.contains("working tree clean"));
    }

    #[test]
    fn render_diff_report_uses_review_oriented_layout() {
        let dir = TempDir::new("list-files").expect("temp dir");
        init_repo(&dir.path).expect("init repo");
        write_file(dir.path.join("tracked.txt"), "one\n").expect("write tracked");
        git(&dir.path, &["add", "tracked.txt"]).expect("git add");
        git(&dir.path, &["commit", "-qm", "init"]).expect("git commit");
        write_file(dir.path.join("tracked.txt"), "one\ntwo\n").expect("modify tracked");
        write_file(dir.path.join("new.txt"), "hello\n").expect("new file");

        let report = collect_workspace_diff(Path::new(&dir.path), true, &no_filters())
            .expect("collect diff");
        let rendered = render_diff_report(&report, render_options(false, false, false));
        assert!(rendered.contains("changed 2 files"));
        assert!(rendered.contains("1 unstaged"));
        assert!(rendered.contains("1 untracked"));
        assert!(rendered.contains("st  scope"));
        assert!(rendered.contains("+add"));
        assert!(rendered.contains("-del"));
        assert!(!rendered.contains("risk"));
        assert!(rendered.contains("stat"));
        assert!(rendered.contains(" M  unstaged"));
        assert!(rendered.contains(" ?  untracked"));
        assert!(rendered.contains("tracked.txt"));
        assert!(rendered.contains("new.txt"));
        assert!(rendered.contains("+"));
    }

    #[test]
    fn render_diff_report_adds_ansi_colors_when_enabled() {
        let report = DiffWorkspaceOutput {
            schema_version: 1,
            repo_root: "/tmp/repo".to_string(),
            head: Some("abc1234".to_string()),
            clean: false,
            filters: DiffFilterSummary::default(),
            risk_policy: DiffRiskPolicy {
                large_threshold: 400,
                large_threshold_source: DiffLargeThresholdSource::FixedFallback,
                large_threshold_history_samples: None,
                large_threshold_history_commits: None,
            },
            workspace_total: DiffSection {
                files: 1,
                additions: 3,
                deletions: 1,
                binary_files: 0,
                file_stats: Vec::new(),
            },
            staged: DiffSection {
                files: 1,
                additions: 3,
                deletions: 1,
                binary_files: 0,
                file_stats: vec![DiffFileStat {
                    path: "src/main.rs".to_string(),
                    previous_path: None,
                    renamed_from: None,
                    renamed_to: None,
                    additions: 3,
                    deletions: 1,
                    binary: false,
                    status: DiffStatus::Modified,
                    primary_scope: Some(DiffScope::Staged),
                    scopes: vec![DiffScope::Staged],
                    risks: vec![DiffRisk {
                        kind: DiffRiskKind::Config,
                        level: DiffRiskLevel::Medium,
                    }],
                }],
            },
            unstaged: DiffSection::default(),
            untracked: DiffSection::default(),
            total: DiffSection {
                files: 1,
                additions: 3,
                deletions: 1,
                binary_files: 0,
                file_stats: vec![DiffFileStat {
                    path: "src/main.rs".to_string(),
                    previous_path: None,
                    renamed_from: None,
                    renamed_to: None,
                    additions: 3,
                    deletions: 1,
                    binary: false,
                    status: DiffStatus::Modified,
                    primary_scope: Some(DiffScope::Staged),
                    scopes: vec![DiffScope::Staged],
                    risks: vec![DiffRisk {
                        kind: DiffRiskKind::Config,
                        level: DiffRiskLevel::Medium,
                    }],
                }],
            },
        };

        let rendered = render_diff_report(&report, render_options(true, true, false));
        assert!(rendered.contains("\u{1b}[32m      +3\u{1b}[0m"));
        assert!(rendered.contains("\u{1b}[31m      -1\u{1b}[0m"));
        assert!(rendered.contains("\u{1b}[33;1m M\u{1b}[0m"));
        assert!(rendered.contains("\u{1b}[32m"));
        assert!(rendered.contains(DIFF_STAT_FILLED_BLOCK));
        assert!(rendered.contains("!"));
        assert!(!rendered.contains("risk"));
    }

    #[test]
    fn collect_workspace_diff_applies_scope_and_path_filters() {
        let dir = TempDir::new("filtered").expect("temp dir");
        init_repo(&dir.path).expect("init repo");
        write_file(dir.path.join("src/main.rs"), "fn main() {}\n").expect("write src");
        write_file(dir.path.join("README.md"), "hello\n").expect("write readme");
        git(&dir.path, &["add", "."]).expect("git add");
        git(&dir.path, &["commit", "-qm", "init"]).expect("git commit");

        write_file(
            dir.path.join("src/main.rs"),
            "fn main() {\n    println!(\"hi\");\n}\n",
        )
        .expect("modify src");
        write_file(dir.path.join("README.md"), "hello\nworld\n").expect("modify readme");

        let filters = DiffFilterSpec {
            summary: DiffFilterSummary {
                scopes: vec![DiffScope::Unstaged],
                path_patterns: vec!["src/**".to_string()],
                exclude_risks: Vec::new(),
            },
            path_matcher: super::build_path_matcher(Path::new(&dir.path), &["src/**".to_string()])
                .expect("build matcher"),
        };
        let report =
            collect_workspace_diff(Path::new(&dir.path), true, &filters).expect("collect diff");
        assert_eq!(report.total.files, 1);
        assert_eq!(report.total.file_stats[0].path, "src/main.rs");
    }

    #[test]
    fn collect_workspace_diff_can_exclude_lockfiles() {
        let dir = TempDir::new("risk-filter").expect("temp dir");
        init_repo(&dir.path).expect("init repo");
        write_file(
            dir.path.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .expect("write manifest");
        write_file(dir.path.join("Cargo.lock"), "lock\n").expect("write lock");
        git(&dir.path, &["add", "."]).expect("git add");
        git(&dir.path, &["commit", "-qm", "init"]).expect("git commit");

        write_file(dir.path.join("Cargo.lock"), "lock\nmore\n").expect("modify lock");
        write_file(
            dir.path.join(".github/workflows/ci.yml"),
            "name: ci\non: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n",
        )
        .expect("write workflow");

        let filters = DiffFilterSpec {
            summary: DiffFilterSummary {
                scopes: Vec::new(),
                path_patterns: Vec::new(),
                exclude_risks: vec![DiffRiskKind::Lockfile],
            },
            path_matcher: None,
        };
        let report =
            collect_workspace_diff(Path::new(&dir.path), true, &filters).expect("collect diff");
        assert!(
            report
                .total
                .file_stats
                .iter()
                .all(|entry| entry.path != "Cargo.lock")
        );
        assert!(
            report
                .total
                .file_stats
                .iter()
                .any(|entry| entry.risks.iter().any(|risk| risk.kind == DiffRiskKind::Ci))
        );
    }

    fn init_repo(path: &Path) -> Result<()> {
        init_empty_repo(path)?;
        git(path, &["config", "user.email", "za@example.com"])?;
        git(path, &["config", "user.name", "za"])?;
        Ok(())
    }

    fn init_empty_repo(path: &Path) -> Result<()> {
        git(path, &["init", "-q"])?;
        Ok(())
    }

    fn git(path: &Path, args: &[&str]) -> Result<()> {
        let status = Command::new("git").args(args).current_dir(path).status()?;
        if !status.success() {
            anyhow::bail!("git command failed: git {}", args.join(" "));
        }
        Ok(())
    }

    fn write_file(path: PathBuf, content: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, content)?;
        Ok(())
    }

    fn no_filters() -> DiffFilterSpec {
        DiffFilterSpec {
            summary: DiffFilterSummary::default(),
            path_matcher: None,
        }
    }

    fn render_options(use_color: bool, use_unicode_stat: bool, name_only: bool) -> RenderOptions {
        RenderOptions {
            use_color,
            use_unicode_stat,
            name_only,
            terminal_width: Some(120),
            interactive: true,
        }
    }
}
