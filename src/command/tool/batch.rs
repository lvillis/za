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
            "No managed tools installed in {} scope.",
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
    run_tool_batch(home, kind, specs, dry_run, verbose, None)
}

pub(super) fn update_tools(
    home: &ToolHome,
    all: bool,
    tools: &[String],
    version: Option<&str>,
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

    let requested_names = if all || tools.is_empty() {
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

    let specs = requested_names
        .iter()
        .map(|name| ToolSpec::from_args(name, version))
        .collect::<Result<Vec<_>>>()?;
    run_tool_batch(home, ToolBatchKind::Update, specs, dry_run, verbose, None)
}

pub(super) fn run_tool_batch(
    home: &ToolHome,
    kind: ToolBatchKind,
    specs: Vec<ToolSpec>,
    dry_run: bool,
    verbose: bool,
    source_label: Option<&str>,
) -> Result<()> {
    let total = specs.len();
    let batch_mode = total > 1 || matches!(kind, ToolBatchKind::Update | ToolBatchKind::Sync);
    let compact_mode = batch_mode && !verbose;
    let mut summary = ToolBatchSummary::default();
    let mut failed_tools = Vec::new();

    if compact_mode {
        print_tool_stage(
            batch_kind_stage(kind),
            batch_start_message(kind, total, source_label),
        );
    }

    let latest_lookup = resolve_batch_latest_lookup(&specs)?;

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
        .emit_stages(!compact_mode);

        match install(home, resolved_spec, options) {
            Ok(result) => {
                summary = summary.record(result.outcome);
                if compact_mode && result.outcome != InstallOutcome::Unchanged {
                    let (stage, message) = render_compact_batch_result(kind, &result, dry_run);
                    print_tool_stage(stage, message);
                }
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

fn resolve_batch_latest_lookup(specs: &[ToolSpec]) -> Result<Option<HashMap<String, LatestCheck>>> {
    let unresolved_names = specs
        .iter()
        .filter(|spec| spec.version.is_none())
        .map(|spec| spec.name.clone())
        .collect::<Vec<_>>();
    if unresolved_names.is_empty() {
        return Ok(None);
    }
    let lookup = resolve_latest_checks_for_names(&unresolved_names)?;
    Ok(Some(lookup.latest_by_name))
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

pub(crate) fn render_compact_batch_result(
    kind: ToolBatchKind,
    result: &InstallResult,
    dry_run: bool,
) -> (&'static str, String) {
    let name = styled_tool_ref(&result.tool.name);
    match result.outcome {
        InstallOutcome::Updated | InstallOutcome::Installed => {
            let stage = batch_kind_stage(kind);
            let message = match result.previous_active.as_deref() {
                Some(previous)
                    if normalize_version(previous) != normalize_version(&result.tool.version) =>
                {
                    if dry_run {
                        format!(
                            "{name} {} {} {} {}",
                            tty_style::dim(previous),
                            tty_style::dim("->"),
                            styled_tool_version(&result.tool.version),
                            tty_style::dim("(dry-run)")
                        )
                    } else {
                        format!(
                            "{name} {} {} {}",
                            tty_style::dim(previous),
                            tty_style::dim("->"),
                            styled_tool_version(&result.tool.version)
                        )
                    }
                }
                _ => {
                    if dry_run {
                        format!(
                            "{name} {} {}",
                            styled_tool_version(&result.tool.version),
                            tty_style::dim("(dry-run)")
                        )
                    } else {
                        format!("{name} {}", styled_tool_version(&result.tool.version))
                    }
                }
            };
            (stage, message)
        }
        InstallOutcome::Repaired => (
            "repair",
            if dry_run {
                format!(
                    "{name} {} {}",
                    tty_style::warning(&result.tool.version),
                    tty_style::dim("(dry-run)")
                )
            } else {
                format!("{name} {}", tty_style::warning(&result.tool.version))
            },
        ),
        InstallOutcome::Unchanged => (
            batch_kind_stage(kind),
            format!(
                "{name} {} {}",
                tty_style::dim("already at"),
                tty_style::dim(&result.tool.version)
            ),
        ),
    }
}

fn styled_tool_ref(name: &str) -> String {
    tty_style::header(format!("`{name}`"))
}

fn styled_tool_version(version: &str) -> String {
    tty_style::active(version)
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
            tty_style::dim("no managed tools changed")
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
        let canonical = canonical_tool_name(&ToolSpec::from_args(name, None)?.name);
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
        parsed.name = canonical_tool_name(&parsed.name);

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
