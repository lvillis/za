use super::render::{render_latest_lines, render_latest_toml};
use super::*;

#[derive(Debug, Clone)]
pub(crate) struct LatestQuery {
    pub(super) name: String,
    pub(super) requirement: Option<String>,
    pub(super) kinds: Option<String>,
    pub(super) source: LatestQuerySource,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LatestQuerySource {
    Args,
    Manifest,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LatestStatus {
    Resolved,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LatestSuggestionKind {
    Add,
    Keep,
    Bump,
    Review,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct LatestRecord {
    pub(super) name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) requirement: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) kinds: Option<String>,
    pub(super) source: LatestQuerySource,
    pub(super) status: LatestStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) latest_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) suggestion_kind: Option<LatestSuggestionKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) suggested_requirement: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) suggestion_note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub(crate) struct LatestSummary {
    pub(super) total: usize,
    pub(super) resolved: usize,
    pub(super) failed: usize,
}

#[derive(Debug, Serialize)]
pub(super) struct LatestReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) manifest_path: Option<String>,
    pub(super) summary: LatestSummary,
    pub(super) records: Vec<LatestRecord>,
}

pub(super) fn run_latest(opts: DepsLatestOptions) -> Result<()> {
    let DepsLatestOptions {
        crates,
        manifest_path,
        jobs,
        include_dev,
        include_build,
        include_optional,
        json,
        toml,
        suggest,
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
            "Resolving latest stable versions{} for {} crate(s) with {} workers...",
            if suggest { " and upgrade guidance" } else { "" },
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
        for line in render_latest_lines(manifest_path.as_deref(), &summary, &records, suggest) {
            println!("{line}");
        }
    }

    let _ = api.flush_cache();
    Ok(())
}

pub(super) fn collect_latest_queries(
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

pub(super) fn resolve_latest_records(
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
            let (suggestion_kind, suggested_requirement, suggestion_note) =
                build_latest_suggestion(&query, &snapshot.max_version);
            LatestRecord {
                name: query.name,
                requirement: query.requirement,
                kinds: query.kinds,
                source: query.source,
                status: LatestStatus::Resolved,
                latest_version: Some(snapshot.max_version),
                suggestion_kind,
                suggested_requirement,
                note: (!notes.is_empty()).then(|| notes.join("; ")),
                suggestion_note,
            }
        }
        Err(err) => LatestRecord {
            name: query.name,
            requirement: query.requirement,
            kinds: query.kinds,
            source: query.source,
            status: LatestStatus::Failed,
            latest_version: None,
            suggestion_kind: None,
            suggested_requirement: None,
            note: Some(format!("crates.io query failed: {err}")),
            suggestion_note: None,
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

fn build_latest_suggestion(
    query: &LatestQuery,
    latest_version: &str,
) -> (Option<LatestSuggestionKind>, Option<String>, Option<String>) {
    let latest = match Version::parse(latest_version) {
        Ok(version) => version,
        Err(_) => {
            return (
                Some(LatestSuggestionKind::Review),
                Some(latest_version.to_string()),
                Some("latest version format needs manual review".to_string()),
            );
        }
    };

    if matches!(query.source, LatestQuerySource::Args) {
        return (
            Some(LatestSuggestionKind::Add),
            Some(latest_version.to_string()),
            Some("explicit query; add this version if needed".to_string()),
        );
    }

    let raw_requirement = query.requirement.as_deref().unwrap_or("-").trim();
    if raw_requirement.is_empty() || raw_requirement == "-" {
        return (
            Some(LatestSuggestionKind::Add),
            Some(latest_version.to_string()),
            Some("dependency has no explicit manifest requirement".to_string()),
        );
    }
    if raw_requirement.contains('|') || raw_requirement.contains("workspace") {
        return (
            Some(LatestSuggestionKind::Review),
            Some(latest_version.to_string()),
            Some("complex manifest requirement; review manually".to_string()),
        );
    }

    let requirement = match VersionReq::parse(raw_requirement) {
        Ok(requirement) => requirement,
        Err(_) => {
            return (
                Some(LatestSuggestionKind::Review),
                Some(latest_version.to_string()),
                Some("manifest requirement is not a plain semver range".to_string()),
            );
        }
    };

    if requirement.matches(&latest) {
        return (
            Some(LatestSuggestionKind::Keep),
            None,
            Some("current requirement already accepts latest".to_string()),
        );
    }

    if requirement_series(&requirement) == Some(version_series(&latest)) {
        return (
            Some(LatestSuggestionKind::Bump),
            Some(latest_version.to_string()),
            Some("same release line; refresh manifest requirement".to_string()),
        );
    }

    (
        Some(LatestSuggestionKind::Review),
        Some(latest_version.to_string()),
        Some("major or nontrivial upgrade; review compatibility".to_string()),
    )
}

fn version_series(version: &Version) -> (u64, Option<u64>) {
    if version.major == 0 {
        (0, Some(version.minor))
    } else {
        (version.major, None)
    }
}

fn requirement_series(requirement: &VersionReq) -> Option<(u64, Option<u64>)> {
    let mut detected = None::<(u64, Option<u64>)>;
    for comparator in &requirement.comparators {
        let series = if comparator.major == 0 {
            (0, comparator.minor)
        } else {
            (comparator.major, None)
        };
        match detected {
            Some(existing) if existing != series => return None,
            Some(_) => {}
            None => detected = Some(series),
        }
    }
    detected
}
