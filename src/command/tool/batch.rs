use super::*;

pub(super) fn install_tools(
    home: &ToolHome,
    tools: &[String],
    version: Option<&str>,
    adopt: bool,
    action: ToolAction,
    dry_run: bool,
    verbose: bool,
) -> Result<()> {
    if adopt && version.is_some() {
        bail!("`za tool install --adopt` does not accept `--version`");
    }
    if adopt && tools.len() != 1 {
        bail!("`za tool install --adopt` requires exactly one tool name");
    }
    if version.is_some() && tools.len() != 1 {
        let command = match action {
            ToolAction::Install => "install",
            ToolAction::Update => "update",
        };
        bail!("`za tool {command} --version` requires exactly one tool name");
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
            "No za-managed tools are installed in {} scope.",
            home.scope.label()
        );
        return Ok(());
    }

    let total = requested_names.len();
    if adopt {
        for (idx, name) in requested_names.iter().enumerate() {
            if total > 1 {
                println!("➡️  [{}/{}] {}", idx + 1, total, name);
            }
            adopt_tool(home, name, dry_run)?;
        }
        return Ok(());
    }

    let specs = requested_names
        .iter()
        .map(|name| ToolSpec::from_args(name, version))
        .collect::<Result<Vec<_>>>()?;
    let kind = match action {
        ToolAction::Install => ToolBatchKind::Install,
        ToolAction::Update => ToolBatchKind::Update,
    };
    run_tool_batch(
        home,
        kind,
        specs,
        dry_run,
        verbose,
        None,
        ToolUpdateChannel::Stable,
    )
}

pub(super) fn update_tools(
    home: &ToolHome,
    all: bool,
    tools: &[String],
    version: Option<&str>,
    alpha: bool,
    dry_run: bool,
    verbose: bool,
) -> Result<()> {
    if all && !tools.is_empty() {
        bail!("`za tool update --all` does not accept tool names");
    }
    if all && version.is_some() {
        bail!("`za tool update --all` does not accept `--version`");
    }
    if version.is_some() && tools.len() != 1 {
        bail!("`za tool update --version` requires exactly one tool name");
    }
    let channel = resolve_update_channel_request(all, tools, version, alpha)?;

    let requested_names = if all || tools.is_empty() {
        collect_managed_tool_names(home)?
    } else {
        normalize_requested_tool_names(tools)?
    };

    if requested_names.is_empty() {
        println!(
            "No za-managed tools are installed in {} scope.",
            home.scope.label()
        );
        return Ok(());
    }

    let specs = requested_names
        .iter()
        .map(|name| ToolSpec::from_args(name, version))
        .collect::<Result<Vec<_>>>()?;
    run_tool_batch(
        home,
        ToolBatchKind::Update,
        specs,
        dry_run,
        verbose,
        None,
        channel,
    )
}

pub(super) fn run_tool_batch(
    home: &ToolHome,
    kind: ToolBatchKind,
    specs: Vec<ToolSpec>,
    dry_run: bool,
    verbose: bool,
    source_label: Option<&str>,
    update_channel: ToolUpdateChannel,
) -> Result<()> {
    let total = specs.len();
    let batch_mode = total > 1 || matches!(kind, ToolBatchKind::Update | ToolBatchKind::Sync);
    let compact_mode = batch_mode && !verbose;
    let parallel_materialize = should_parallel_materialize_batch(total, dry_run, verbose);
    let mut summary = ToolBatchSummary::default();
    let mut failed_tools = Vec::new();
    let mut materialize_tasks = Vec::new();

    if compact_mode {
        print_tool_stage(
            batch_kind_stage(kind),
            batch_start_message(kind, total, source_label),
        );
    }

    let latest_lookup = resolve_batch_latest_lookup(&specs, update_channel)?;

    for (idx, requested) in specs.iter().enumerate() {
        ensure_not_interrupted()?;
        if batch_mode && !compact_mode {
            println!("➡️  [{}/{}] {}", idx + 1, total, requested.name);
        }

        let resolved_spec = match resolve_batch_tool_spec(requested, latest_lookup.as_ref()) {
            Ok(spec) => spec,
            Err(err) => {
                summary.failed += 1;
                failed_tools.push(requested.name.clone());
                let message = if compact_mode {
                    summarize_tool_update_error(&err.to_string())
                } else {
                    err.to_string()
                };
                print_tool_stage("fail", format!("`{}` {message}", requested.name));
                if total == 1 {
                    return Err(err);
                }
                continue;
            }
        };

        let options = match kind {
            ToolBatchKind::Install => InstallOptions::install(za_config::ProxyScope::Tool),
            ToolBatchKind::Update | ToolBatchKind::Sync => {
                InstallOptions::update(za_config::ProxyScope::Tool)
            }
        }
        .dry_run(dry_run)
        .emit_stages(!compact_mode)
        .emit_plan_stage(compact_mode)
        .download_display(if compact_mode {
            source::DownloadDisplay::Compact
        } else {
            source::DownloadDisplay::Detailed
        });

        if parallel_materialize {
            match plan_install(home, resolved_spec, options) {
                Ok(plan) => {
                    emit_install_plan_stage(
                        &plan.tool,
                        plan.previous_active.as_deref(),
                        plan.planned_outcome,
                        plan.current_matches_target,
                        options,
                    );
                    if update_plan_is_unchanged(&plan, options) {
                        summary = summary.record(InstallOutcome::Unchanged);
                        continue;
                    }
                    materialize_tasks.push(BatchInstallTask {
                        index: idx,
                        requested_name: requested.name.clone(),
                        plan,
                        materialize_options: options
                            .emit_stages(false)
                            .emit_plan_stage(false)
                            .download_display(source::DownloadDisplay::Quiet),
                        activate_options: options,
                    });
                }
                Err(err) => {
                    summary.failed += 1;
                    failed_tools.push(requested.name.clone());
                    let message = if compact_mode {
                        summarize_tool_update_error(&err.to_string())
                    } else {
                        err.to_string()
                    };
                    print_tool_stage("fail", format!("`{}` {message}", requested.name));
                }
            }
            continue;
        }

        match install(home, resolved_spec, options) {
            Ok(result) => {
                summary = summary.record(result.outcome);
            }
            Err(err) => {
                summary.failed += 1;
                failed_tools.push(requested.name.clone());
                let message = if compact_mode {
                    summarize_tool_update_error(&err.to_string())
                } else {
                    err.to_string()
                };
                print_tool_stage("fail", format!("`{}` {message}", requested.name));
                if total == 1 {
                    return Err(err);
                }
            }
        }
    }

    if parallel_materialize && !materialize_tasks.is_empty() {
        for result in materialize_tool_batch_parallel(home, kind, materialize_tasks) {
            if let Some(err) = result.error {
                summary.failed += 1;
                failed_tools.push(result.requested_name.clone());
                let message = if compact_mode {
                    summarize_tool_update_error(&err.to_string())
                } else {
                    err.to_string()
                };
                print_tool_stage("fail", format!("`{}` {message}", result.requested_name));
                continue;
            }

            let Some(plan) = result.plan else {
                summary.failed += 1;
                failed_tools.push(result.requested_name.clone());
                print_tool_stage(
                    "fail",
                    format!(
                        "`{}` internal materialize result missing",
                        result.requested_name
                    ),
                );
                continue;
            };
            match activate_install_plan(home, &plan, result.activate_options) {
                Ok(()) => summary = summary.record(plan.planned_outcome),
                Err(err) => {
                    summary.failed += 1;
                    failed_tools.push(result.requested_name.clone());
                    let message = if compact_mode {
                        summarize_tool_update_error(&err.to_string())
                    } else {
                        err.to_string()
                    };
                    print_tool_stage("fail", format!("`{}` {message}", result.requested_name));
                }
            }
        }
    }

    if batch_mode {
        print_tool_stage("done", render_batch_summary(kind, summary, dry_run));
    }

    if failed_tools.is_empty() {
        return Ok(());
    }

    bail!(
        "{} finished with {} failure(s): {}",
        batch_kind_noun(kind),
        failed_tools.len(),
        failed_tools.join(", ")
    )
}

#[derive(Clone)]
struct BatchInstallTask {
    index: usize,
    requested_name: String,
    plan: InstallPlan,
    materialize_options: InstallOptions,
    activate_options: InstallOptions,
}

struct BatchInstallResult {
    index: usize,
    requested_name: String,
    plan: Option<InstallPlan>,
    activate_options: InstallOptions,
    error: Option<anyhow::Error>,
}

pub(super) fn should_parallel_materialize_batch(
    total: usize,
    dry_run: bool,
    verbose: bool,
) -> bool {
    total > 1 && !dry_run && !verbose
}

fn materialize_tool_batch_parallel(
    home: &ToolHome,
    kind: ToolBatchKind,
    tasks: Vec<BatchInstallTask>,
) -> Vec<BatchInstallResult> {
    let worker_count = materialize_worker_count(tasks.len());
    let progress_ui = BatchProgressUi::new(kind, &tasks);
    let queue = Arc::new(Mutex::new(VecDeque::from(tasks)));
    let out: Arc<Mutex<Vec<BatchInstallResult>>> = Arc::new(Mutex::new(Vec::new()));
    let progress_sink = progress_ui.as_ref().map(|progress| {
        let progress = Arc::clone(progress);
        Arc::new(move |event: source::DownloadProgress| {
            progress.record_download(event);
        }) as source::DownloadProgressSink
    });

    thread::scope(|scope| {
        for _ in 0..worker_count {
            let home = home.clone();
            let queue = Arc::clone(&queue);
            let out = Arc::clone(&out);
            let progress_ui = progress_ui.clone();
            let progress_sink = progress_sink.clone();
            scope.spawn(move || {
                loop {
                    let task = match queue.lock() {
                        Ok(mut guard) => guard.pop_front(),
                        Err(_) => None,
                    };
                    let Some(task) = task else {
                        break;
                    };
                    if let Some(progress) = progress_ui.as_ref() {
                        progress
                            .set_tool_status(&task.plan.tool.name, BatchProgressStatus::Preparing);
                    }
                    let tool_name = task.plan.tool.name.clone();
                    let materialized = materialize_install_plan(
                        &home,
                        &task.plan,
                        task.materialize_options,
                        progress_sink.clone(),
                    );
                    let result = match materialized {
                        Ok(()) => {
                            if let Some(progress) = progress_ui.as_ref() {
                                progress.finish_tool(
                                    tool_name.as_str(),
                                    BatchProgressTerminalStatus::Ready,
                                );
                            }
                            BatchInstallResult {
                                index: task.index,
                                requested_name: task.requested_name,
                                plan: Some(task.plan),
                                activate_options: task.activate_options,
                                error: None,
                            }
                        }
                        Err(err) => {
                            if let Some(progress) = progress_ui.as_ref() {
                                progress.finish_tool(
                                    tool_name.as_str(),
                                    BatchProgressTerminalStatus::Failed,
                                );
                            }
                            BatchInstallResult {
                                index: task.index,
                                requested_name: task.requested_name,
                                plan: None,
                                activate_options: task.activate_options,
                                error: Some(err),
                            }
                        }
                    };
                    if let Ok(mut guard) = out.lock() {
                        guard.push(result);
                    } else {
                        break;
                    }
                }
            });
        }
    });

    if let Some(progress) = progress_ui {
        progress.clear();
    }

    let mut results = out
        .lock()
        .map(|mut guard| std::mem::take(&mut *guard))
        .unwrap_or_default();
    results.sort_by_key(|result| result.index);
    results
}

struct BatchProgressUi {
    kind: ToolBatchKind,
    total: usize,
    completed: Mutex<usize>,
    multi: MultiProgress,
    header: ProgressBar,
    lines: Mutex<HashMap<String, ProgressBar>>,
}

impl BatchProgressUi {
    fn new(kind: ToolBatchKind, tasks: &[BatchInstallTask]) -> Option<Arc<Self>> {
        if tasks.is_empty() || !io::stderr().is_terminal() {
            return None;
        }

        let total = tasks.len();
        let multi = MultiProgress::with_draw_target(ProgressDrawTarget::stderr_with_hz(10));
        let header = multi.add(new_batch_header_progress_bar(kind, 0, total));
        let mut lines = HashMap::new();
        for task in tasks {
            let name = task.plan.tool.name.clone();
            let progress = multi.add(new_batch_tool_progress_bar(
                &name,
                BatchProgressStatus::Queued,
            ));
            lines.insert(name, progress);
        }

        Some(Arc::new(Self {
            kind,
            total,
            completed: Mutex::new(0),
            multi,
            header,
            lines: Mutex::new(lines),
        }))
    }

    fn set_tool_status(&self, tool_name: &str, status: BatchProgressStatus) {
        let Ok(lines) = self.lines.lock() else {
            return;
        };
        if let Some(progress) = lines.get(tool_name) {
            progress.set_message(render_batch_progress_line(tool_name, status));
        }
    }

    fn record_download(&self, event: source::DownloadProgress) {
        if event.finished {
            return;
        }
        self.set_tool_status(
            &event.tool,
            BatchProgressStatus::Downloading {
                downloaded: event.downloaded,
                total_bytes: event.total_bytes,
                elapsed: event.elapsed,
            },
        );
    }

    fn finish_tool(&self, tool_name: &str, status: BatchProgressTerminalStatus) {
        let completed = self.mark_completed();
        self.header.set_message(render_batch_progress_header(
            self.kind, completed, self.total,
        ));

        let Ok(lines) = self.lines.lock() else {
            return;
        };
        let Some(progress) = lines.get(tool_name) else {
            return;
        };
        let line = render_batch_terminal_progress_line(tool_name, status);
        match status {
            BatchProgressTerminalStatus::Ready => progress.finish_with_message(line),
            BatchProgressTerminalStatus::Failed => progress.abandon_with_message(line),
        }
    }

    fn mark_completed(&self) -> usize {
        let Ok(mut completed) = self.completed.lock() else {
            return 0;
        };
        *completed += 1;
        *completed
    }

    fn clear(&self) {
        if let Ok(lines) = self.lines.lock() {
            for progress in lines.values() {
                progress.finish_and_clear();
            }
        }
        self.header.finish_and_clear();
        let _ = self.multi.clear();
    }
}

#[derive(Clone, Copy)]
pub(super) enum BatchProgressStatus {
    Queued,
    Preparing,
    Downloading {
        downloaded: u64,
        total_bytes: Option<u64>,
        elapsed: Duration,
    },
}

#[derive(Clone, Copy)]
enum BatchProgressTerminalStatus {
    Ready,
    Failed,
}

fn new_batch_header_progress_bar(
    kind: ToolBatchKind,
    completed: usize,
    total: usize,
) -> ProgressBar {
    let progress = ProgressBar::new_spinner();
    progress.set_style(
        ProgressStyle::with_template("{wide_msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner()),
    );
    progress.set_message(render_batch_progress_header(kind, completed, total));
    progress
}

fn new_batch_tool_progress_bar(tool_name: &str, status: BatchProgressStatus) -> ProgressBar {
    let progress = ProgressBar::new_spinner();
    progress.set_style(
        ProgressStyle::with_template("{spinner} {wide_msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner())
            .tick_strings(&["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"]),
    );
    progress.enable_steady_tick(Duration::from_millis(80));
    progress.set_message(render_batch_progress_line(tool_name, status));
    progress
}

pub(super) fn render_batch_progress_header(
    kind: ToolBatchKind,
    completed: usize,
    total: usize,
) -> String {
    format!(
        "[+] {} {}/{}",
        match kind {
            ToolBatchKind::Install => "Installing",
            ToolBatchKind::Update => "Updating",
            ToolBatchKind::Sync => "Syncing",
        },
        completed.min(total),
        total
    )
}

pub(super) fn render_batch_progress_line(tool_name: &str, status: BatchProgressStatus) -> String {
    match status {
        BatchProgressStatus::Queued => format!("{tool_name:<12} queued"),
        BatchProgressStatus::Preparing => format!("{tool_name:<12} preparing"),
        BatchProgressStatus::Downloading {
            downloaded,
            total_bytes,
            elapsed,
        } => format!(
            "{tool_name:<12} {}",
            source::render_download_progress_brief(downloaded, total_bytes, elapsed)
        ),
    }
}

fn render_batch_terminal_progress_line(
    tool_name: &str,
    status: BatchProgressTerminalStatus,
) -> String {
    match status {
        BatchProgressTerminalStatus::Ready => format!("✔ {tool_name:<12} downloaded"),
        BatchProgressTerminalStatus::Failed => format!("✘ {tool_name:<12} failed"),
    }
}

fn materialize_worker_count(task_count: usize) -> usize {
    thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(TOOL_UPDATE_JOBS_MIN)
        .clamp(TOOL_UPDATE_JOBS_MIN, TOOL_MATERIALIZE_JOBS_MAX)
        .min(task_count.max(1))
}

pub(super) fn resolve_update_channel_request(
    all: bool,
    tools: &[String],
    version: Option<&str>,
    alpha: bool,
) -> Result<ToolUpdateChannel> {
    if !alpha {
        return Ok(ToolUpdateChannel::Stable);
    }
    if all {
        bail!("`za tool update --alpha` does not accept `--all`");
    }
    if version.is_some() {
        bail!("`za tool update --alpha` does not accept `--version`");
    }
    if tools.len() != 1 {
        bail!("`za tool update --alpha` requires exactly one tool name: codex");
    }

    let canonical = canonical_tool_name(&ToolSpec::from_args(&tools[0], None)?.name);
    if canonical != "codex" {
        bail!("`za tool update --alpha` is only supported for `codex`");
    }
    Ok(ToolUpdateChannel::CodexAlpha)
}

fn resolve_batch_latest_lookup(
    specs: &[ToolSpec],
    update_channel: ToolUpdateChannel,
) -> Result<Option<HashMap<String, LatestCheck>>> {
    let unresolved_names = specs
        .iter()
        .filter(|spec| spec.version.is_none())
        .map(|spec| spec.name.clone())
        .collect::<Vec<_>>();
    if unresolved_names.is_empty() {
        return Ok(None);
    }
    if update_channel == ToolUpdateChannel::CodexAlpha {
        let version = source::fetch_latest_codex_alpha_version(za_config::ProxyScope::Tool)?;
        let latest_by_name = unresolved_names
            .into_iter()
            .map(|name| (name, LatestCheck::Latest(version.clone())))
            .collect::<HashMap<_, _>>();
        return Ok(Some(latest_by_name));
    }
    let lookup = resolve_latest_checks_for_names_with_mode(
        &unresolved_names,
        latest_resolution_mode_for_batch(specs, update_channel),
    )?;
    Ok(Some(lookup.latest_by_name))
}

pub(super) fn latest_resolution_mode_for_batch(
    specs: &[ToolSpec],
    update_channel: ToolUpdateChannel,
) -> LatestResolutionMode {
    if update_channel != ToolUpdateChannel::Stable {
        return LatestResolutionMode::Exact;
    }
    if specs.len() == 1 && canonical_tool_name(&specs[0].name) == "codex" {
        LatestResolutionMode::Exact
    } else {
        LatestResolutionMode::Fast
    }
}

fn resolve_batch_tool_spec(
    requested: &ToolSpec,
    latest_lookup: Option<&HashMap<String, LatestCheck>>,
) -> Result<ToolSpec> {
    if requested.version.is_some() {
        return Ok(requested.clone());
    }

    match latest_lookup
        .and_then(|lookup| lookup.get(&requested.name))
        .cloned()
        .unwrap_or(LatestCheck::Unsupported)
    {
        LatestCheck::Latest(version) => ToolSpec::from_args(&requested.name, Some(&version)),
        LatestCheck::Error(err) => Err(anyhow!(err)),
        LatestCheck::Unsupported => bail!(
            "latest version resolution is not supported for `{}`",
            requested.name
        ),
    }
}

fn batch_kind_stage(kind: ToolBatchKind) -> &'static str {
    match kind {
        ToolBatchKind::Install => "install",
        ToolBatchKind::Update => "update",
        ToolBatchKind::Sync => "sync",
    }
}

fn batch_kind_noun(kind: ToolBatchKind) -> &'static str {
    match kind {
        ToolBatchKind::Install => "tool install",
        ToolBatchKind::Update => "tool update",
        ToolBatchKind::Sync => "tool sync",
    }
}

fn batch_start_message(kind: ToolBatchKind, total: usize, source_label: Option<&str>) -> String {
    match kind {
        ToolBatchKind::Install => format!("preparing {total} tool(s)"),
        ToolBatchKind::Update => format!("checking {total} managed tool(s)"),
        ToolBatchKind::Sync => match source_label {
            Some(label) => format!("syncing {total} tool(s) from {label}"),
            None => format!("syncing {total} tool(s)"),
        },
    }
}

pub(crate) fn render_batch_summary(
    kind: ToolBatchKind,
    summary: ToolBatchSummary,
    dry_run: bool,
) -> String {
    let mut parts = Vec::new();
    match kind {
        ToolBatchKind::Install => {
            if summary.installed > 0 {
                parts.push(tool_summary_token(
                    summary.installed,
                    if dry_run {
                        "would install"
                    } else {
                        "installed"
                    },
                    "active",
                ));
            }
            if summary.repaired > 0 {
                parts.push(tool_summary_token(
                    summary.repaired,
                    if dry_run { "would repair" } else { "repaired" },
                    "warning",
                ));
            }
            if summary.unchanged > 0 {
                parts.push(tool_summary_token(
                    summary.unchanged,
                    "already present",
                    "dim",
                ));
            }
        }
        ToolBatchKind::Update => {
            if summary.updated > 0 {
                parts.push(tool_summary_token(
                    summary.updated,
                    if dry_run { "would update" } else { "updated" },
                    "active",
                ));
            }
            if summary.repaired > 0 {
                parts.push(tool_summary_token(
                    summary.repaired,
                    if dry_run { "would repair" } else { "repaired" },
                    "warning",
                ));
            }
            if summary.unchanged > 0 {
                parts.push(tool_summary_token(
                    summary.unchanged,
                    "already latest",
                    "dim",
                ));
            }
        }
        ToolBatchKind::Sync => {
            let synced = summary.installed + summary.updated;
            if synced > 0 {
                parts.push(tool_summary_token(
                    synced,
                    if dry_run { "would sync" } else { "synced" },
                    "active",
                ));
            }
            if summary.repaired > 0 {
                parts.push(tool_summary_token(
                    summary.repaired,
                    if dry_run { "would repair" } else { "repaired" },
                    "warning",
                ));
            }
            if summary.unchanged > 0 {
                parts.push(tool_summary_token(
                    summary.unchanged,
                    "already aligned",
                    "dim",
                ));
            }
        }
    }

    if summary.failed > 0 {
        parts.push(tool_summary_token(summary.failed, "failed", "error"));
    }

    if parts.is_empty() {
        if dry_run {
            tty_style::dim("dry-run complete")
        } else {
            tty_style::dim("no tool changes")
        }
    } else {
        parts.join(", ")
    }
}

fn tool_summary_token(count: usize, label: &str, tone: &str) -> String {
    let token = format!("{count} {label}");
    match tone {
        "active" => tty_style::active(token),
        "warning" => tty_style::warning(token),
        "error" => tty_style::error(token),
        _ => tty_style::dim(token),
    }
}

fn summarize_tool_update_error(err: &str) -> String {
    text_render::truncate_end(err, 160)
}

pub(super) fn normalize_requested_tool_names(names: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for name in names {
        let canonical = canonical_supported_tool_name(&ToolSpec::from_args(name, None)?.name)?;
        if seen.insert(canonical.clone()) {
            out.push(canonical);
        }
    }
    Ok(out)
}

pub(super) fn sync_manifest(
    home: &ToolHome,
    file: &Path,
    dry_run: bool,
    verbose: bool,
) -> Result<()> {
    let specs = load_sync_specs_from_manifest(file)?;
    let source_label = file.display().to_string();
    let parsed = specs
        .iter()
        .map(|spec| ToolSpec::parse(spec))
        .collect::<Result<Vec<_>>>()?;
    run_tool_batch(
        home,
        ToolBatchKind::Sync,
        parsed,
        dry_run,
        verbose,
        Some(&source_label),
        ToolUpdateChannel::Stable,
    )
}

pub(crate) fn load_sync_specs_from_manifest(file: &Path) -> Result<Vec<String>> {
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
            .with_context(|| format!("parse sync spec `{trimmed}` in {}", file.display()))?;
        parsed.name = canonical_supported_tool_name(&parsed.name)
            .with_context(|| format!("validate sync spec `{trimmed}` in {}", file.display()))?;

        let spec = match parsed.version {
            Some(version) => format!("{}:{}", parsed.name, version),
            None => parsed.name,
        };
        if seen.insert(spec.clone()) {
            specs.push(spec);
        }
    }

    Ok(specs)
}
