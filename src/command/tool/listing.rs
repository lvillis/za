use super::*;

#[derive(Debug, Clone)]
pub(super) enum LatestCheck {
    Latest(String),
    Unsupported,
    Error(String),
}

pub(super) fn list_update_status(installed_version: &str, latest: &LatestCheck) -> String {
    match latest {
        LatestCheck::Latest(remote)
            if normalize_version(installed_version) == normalize_version(remote) =>
        {
            "latest".to_string()
        }
        LatestCheck::Latest(remote) => format!("update -> {remote}"),
        LatestCheck::Unsupported => "n/a".to_string(),
        LatestCheck::Error(_) => "check-failed".to_string(),
    }
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct UnmanagedBinary {
    pub(super) name: String,
    pub(super) version: String,
    pub(super) path: String,
}

#[derive(Debug)]
struct InstalledToolRow {
    name: String,
    active_version: Option<String>,
    active_missing_from_store: bool,
    installed_versions: Vec<String>,
    source: Option<String>,
    bin_path: Option<String>,
}

#[derive(Debug)]
struct InstalledToolReport {
    rows: Vec<InstalledToolRow>,
    unmanaged: Vec<UnmanagedBinary>,
    scope: String,
    bin_path: String,
}

#[derive(Debug, Serialize)]
struct InstalledToolReportJson {
    scope: String,
    tool_binaries_path: String,
    rows: Vec<InstalledToolRowJson>,
    unmanaged: Vec<UnmanagedBinary>,
}

#[derive(Debug, Serialize)]
struct InstalledToolRowJson {
    name: String,
    active_version: Option<String>,
    active_missing_from_store: bool,
    installed_versions: Vec<String>,
    installed_count: usize,
    source: Option<String>,
    bin_path: Option<String>,
}

#[derive(Debug, Serialize)]
struct SupportedToolView {
    tool: String,
    aliases: Vec<String>,
    sources: String,
}

#[derive(Debug)]
struct ToolVersionDetail {
    version: String,
    active: bool,
    source: String,
    executable_path: String,
    manifest_path: String,
}

#[derive(Debug)]
struct ToolDetailReport {
    name: String,
    aliases: Vec<String>,
    scope: String,
    managed: bool,
    active_version: Option<String>,
    active_missing_from_store: bool,
    active_bin_path: Option<String>,
    supported_source: Option<String>,
    installed: Vec<ToolVersionDetail>,
    unmanaged: Option<UnmanagedBinary>,
}

#[derive(Debug, Serialize)]
struct ToolVersionDetailJson {
    version: String,
    active: bool,
    source: String,
    executable_path: String,
    manifest_path: String,
}

#[derive(Debug, Serialize)]
struct ToolDetailReportJson {
    name: String,
    aliases: Vec<String>,
    scope: String,
    managed: bool,
    active_version: Option<String>,
    active_missing_from_store: bool,
    active_bin_path: Option<String>,
    supported_source: Option<String>,
    installed: Vec<ToolVersionDetailJson>,
    unmanaged: Option<UnmanagedBinary>,
}

#[derive(Debug)]
struct OutdatedRow {
    name: String,
    version: String,
    active: bool,
    source: String,
    update: String,
    latest: Option<String>,
}

#[derive(Debug)]
struct OutdatedReport {
    rows: Vec<OutdatedRow>,
    unmanaged: Vec<UnmanagedBinary>,
    check_failures: Vec<(String, String)>,
    scope: String,
    bin_path: String,
    has_updates: bool,
}

#[derive(Debug, Serialize)]
struct OutdatedReportJson {
    scope: String,
    tool_binaries_path: String,
    rows: Vec<OutdatedRowJson>,
    unmanaged: Vec<UnmanagedBinary>,
    update_check_warnings: Vec<String>,
    has_updates: bool,
    has_check_failures: bool,
}

#[derive(Debug, Serialize)]
struct OutdatedRowJson {
    name: String,
    version: String,
    active: bool,
    source: String,
    update: String,
    latest: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct ToolUpdateCacheFile {
    schema_version: u32,
    #[serde(default)]
    latest_versions: HashMap<String, ToolUpdateCacheEntry>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ToolUpdateCacheEntry {
    latest_version: String,
    fetched_at_unix_secs: u64,
}

#[derive(Debug)]
struct ToolUpdateCacheState {
    path: Option<PathBuf>,
    data: ToolUpdateCacheFile,
    dirty: bool,
}

impl ToolUpdateCacheState {
    fn load() -> Self {
        let path = tool_update_cache_path();
        let Some(path_ref) = path.as_ref() else {
            return Self {
                path,
                data: ToolUpdateCacheFile {
                    schema_version: TOOL_UPDATE_CACHE_SCHEMA_VERSION,
                    latest_versions: HashMap::new(),
                },
                dirty: false,
            };
        };
        match fs::read(path_ref) {
            Ok(raw) => match serde_json::from_slice::<ToolUpdateCacheFile>(&raw) {
                Ok(mut data) => {
                    if data.schema_version != TOOL_UPDATE_CACHE_SCHEMA_VERSION {
                        data = ToolUpdateCacheFile {
                            schema_version: TOOL_UPDATE_CACHE_SCHEMA_VERSION,
                            latest_versions: HashMap::new(),
                        };
                    }
                    Self {
                        path,
                        data,
                        dirty: false,
                    }
                }
                Err(_) => Self {
                    path,
                    data: ToolUpdateCacheFile {
                        schema_version: TOOL_UPDATE_CACHE_SCHEMA_VERSION,
                        latest_versions: HashMap::new(),
                    },
                    dirty: false,
                },
            },
            Err(_) => Self {
                path,
                data: ToolUpdateCacheFile {
                    schema_version: TOOL_UPDATE_CACHE_SCHEMA_VERSION,
                    latest_versions: HashMap::new(),
                },
                dirty: false,
            },
        }
    }

    fn get_latest_if_fresh(&mut self, key: &str, now_unix_secs: u64) -> Option<String> {
        if let Some(entry) = self.data.latest_versions.get(key) {
            if now_unix_secs.saturating_sub(entry.fetched_at_unix_secs)
                <= TOOL_UPDATE_CACHE_TTL_SECS
            {
                return Some(entry.latest_version.clone());
            }
            self.data.latest_versions.remove(key);
            self.dirty = true;
        }
        None
    }

    fn put_latest(&mut self, key: &str, latest_version: String, now_unix_secs: u64) {
        self.data.latest_versions.insert(
            key.to_string(),
            ToolUpdateCacheEntry {
                latest_version,
                fetched_at_unix_secs: now_unix_secs,
            },
        );
        self.dirty = true;
    }

    fn save_if_dirty(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let Some(path) = self.path.as_ref() else {
            return Ok(());
        };
        let Some(parent) = path.parent() else {
            return Ok(());
        };

        fs::create_dir_all(parent)
            .with_context(|| format!("create tool cache directory {}", parent.display()))?;
        let json = serde_json::to_vec_pretty(&self.data).context("serialize tool update cache")?;
        let tmp = path.with_extension(format!("tmp-tool-cache-{}", std::process::id()));
        fs::write(&tmp, json).with_context(|| format!("write {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| {
            format!("replace tool cache {} -> {}", path.display(), tmp.display())
        })?;
        self.dirty = false;
        Ok(())
    }
}

#[derive(Debug)]
struct LatestLookup {
    latest_by_name: HashMap<String, LatestCheck>,
}

pub(super) fn list_installed(home: &ToolHome, json: bool) -> Result<i32> {
    let report = build_installed_report(home)?;
    if json {
        print_installed_json(&report)?;
    } else {
        print_installed_text(&report);
    }
    Ok(0)
}

pub(super) fn show_tool(home: &ToolHome, tool: &str, json: bool) -> Result<i32> {
    let report = build_tool_detail_report(home, tool)?;
    if json {
        print_tool_detail_json(&report)?;
    } else {
        print_tool_detail_text(&report);
    }
    Ok(0)
}

pub(super) fn show_catalog(json: bool) -> Result<i32> {
    let rows = supported_tools_view();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).context("serialize supported tools JSON")?
        );
    } else {
        print_supported_tools(&rows);
    }
    Ok(0)
}

pub(super) fn list_outdated(
    home: &ToolHome,
    tools: &[String],
    json: bool,
    fail_on_updates: bool,
    fail_on_check_errors: bool,
) -> Result<i32> {
    let names = if tools.is_empty() {
        collect_managed_tool_names(home)?
    } else {
        normalize_requested_tool_names(tools)?
    };

    if names.is_empty() {
        println!(
            "No managed tools installed in {} scope.",
            home.scope.label()
        );
        return Ok(0);
    }

    for name in &names {
        if !has_managed_state(home, name)? {
            bail!("`{}` is not managed in {} scope", name, home.scope.label());
        }
    }

    let report = build_outdated_report(home, &names)?;
    if json {
        print_outdated_json(&report)?;
    } else {
        print_outdated_text(&report);
    }

    if fail_on_check_errors && !report.check_failures.is_empty() {
        eprintln!(
            "tool outdated policy failure: {} update checks failed",
            report.check_failures.len()
        );
        return Ok(TOOL_EXIT_UPDATE_CHECK_FAILED);
    }
    if fail_on_updates && report.has_updates {
        eprintln!("tool outdated policy failure: updates available");
        return Ok(TOOL_EXIT_UPDATES_AVAILABLE);
    }
    Ok(0)
}

fn build_installed_report(home: &ToolHome) -> Result<InstalledToolReport> {
    let mut rows = Vec::new();
    for name in collect_managed_tool_names(home)? {
        let active_version = read_current_version(home, &name)?;
        let mut installed_versions = collect_dir_names(&home.name_dir(&name))?;
        installed_versions.sort();
        if active_version.is_none() && installed_versions.is_empty() {
            continue;
        }

        let active_missing_from_store = active_version
            .as_ref()
            .is_some_and(|active| !installed_versions.iter().any(|version| version == active));
        let source = active_version
            .as_ref()
            .map(|version| {
                manifest_source_label(
                    home,
                    &ToolRef {
                        name: name.clone(),
                        version: version.clone(),
                    },
                )
            })
            .transpose()?;

        rows.push(InstalledToolRow {
            name: name.clone(),
            active_version: active_version.clone(),
            active_missing_from_store,
            installed_versions,
            source,
            bin_path: active_version.map(|_| home.active_path(&name).display().to_string()),
        });
    }

    Ok(InstalledToolReport {
        rows,
        unmanaged: collect_unmanaged_binaries(home)?,
        scope: home.scope.label().to_string(),
        bin_path: home.bin_dir.display().to_string(),
    })
}

fn build_tool_detail_report(home: &ToolHome, tool: &str) -> Result<ToolDetailReport> {
    let name = canonical_tool_name(&ToolSpec::from_args(tool, None)?.name);
    let policy = find_tool_policy(&name);
    let active_version = read_current_version(home, &name)?;
    let mut installed_versions = collect_dir_names(&home.name_dir(&name))?;
    installed_versions.sort();

    let active_missing_from_store = active_version
        .as_ref()
        .is_some_and(|active| !installed_versions.iter().any(|version| version == active));
    let managed = active_version.is_some() || !installed_versions.is_empty();
    let unmanaged = collect_unmanaged_binary_for_name(home, &name)?;

    if !managed && unmanaged.is_none() && policy.is_none() {
        bail!(
            "unknown tool `{}`; supported built-ins: {}",
            name,
            supported_tool_names_csv()
        );
    }

    let installed = installed_versions
        .iter()
        .map(|version| {
            let tool = ToolRef {
                name: name.clone(),
                version: version.clone(),
            };
            Ok(ToolVersionDetail {
                version: version.clone(),
                active: active_version.as_deref() == Some(version.as_str()),
                source: manifest_source_label(home, &tool)?,
                executable_path: home.install_path(&tool).display().to_string(),
                manifest_path: home.manifest_path(&tool).display().to_string(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let active_bin_path = active_version
        .as_ref()
        .map(|_| home.active_path(&name).display().to_string());

    Ok(ToolDetailReport {
        name,
        aliases: policy
            .map(|policy| {
                policy
                    .aliases
                    .iter()
                    .map(|alias| (*alias).to_string())
                    .collect()
            })
            .unwrap_or_default(),
        scope: home.scope.label().to_string(),
        managed,
        active_version: active_version.clone(),
        active_missing_from_store,
        active_bin_path,
        supported_source: policy.map(|policy| policy.source_label.to_string()),
        installed,
        unmanaged,
    })
}

fn build_outdated_report(home: &ToolHome, names: &[String]) -> Result<OutdatedReport> {
    let latest_lookup = resolve_latest_checks_for_names(names)?;
    let mut rows = Vec::new();
    let mut check_failures = Vec::new();
    let mut has_updates = false;

    for name in names {
        let Some((version, active)) = select_primary_version(home, name)? else {
            continue;
        };
        let latest = latest_lookup
            .latest_by_name
            .get(name)
            .cloned()
            .unwrap_or(LatestCheck::Unsupported);
        if let LatestCheck::Error(err) = &latest {
            check_failures.push((name.clone(), source::truncate_for_log(err, 120)));
        }

        let update = list_update_status(&version, &latest);
        let latest_version = match &latest {
            LatestCheck::Latest(remote) => Some(remote.clone()),
            LatestCheck::Unsupported | LatestCheck::Error(_) => None,
        };
        let update_available = matches!(
            latest,
            LatestCheck::Latest(ref remote)
                if normalize_version(&version) != normalize_version(remote)
        );
        has_updates |= update_available;

        rows.push(OutdatedRow {
            name: name.clone(),
            version: version.clone(),
            active,
            source: manifest_source_label(
                home,
                &ToolRef {
                    name: name.clone(),
                    version,
                },
            )?,
            update,
            latest: latest_version,
        });
    }

    Ok(OutdatedReport {
        rows,
        unmanaged: collect_unmanaged_binaries(home)?,
        check_failures,
        scope: home.scope.label().to_string(),
        bin_path: home.bin_dir.display().to_string(),
        has_updates,
    })
}

fn select_primary_version(home: &ToolHome, name: &str) -> Result<Option<(String, bool)>> {
    if let Some(active) = read_current_version(home, name)? {
        return Ok(Some((active, true)));
    }

    let mut installed = collect_dir_names(&home.name_dir(name))?;
    installed.sort();
    Ok(installed.pop().map(|version| (version, false)))
}

fn has_managed_state(home: &ToolHome, name: &str) -> Result<bool> {
    Ok(read_current_version(home, name)?.is_some() || is_name_managed(home, name)?)
}

fn collect_unmanaged_binary_for_name(
    home: &ToolHome,
    name: &str,
) -> Result<Option<UnmanagedBinary>> {
    Ok(collect_unmanaged_binaries(home)?
        .into_iter()
        .find(|item| item.name == name))
}

#[derive(Debug)]
struct InstalledTextRow {
    name: String,
    active: String,
    versions: String,
    source: String,
}

#[derive(Debug)]
struct OutdatedTextRow {
    name: String,
    version: String,
    active: String,
    update: String,
    source: String,
}

fn text_width(value: &str) -> usize {
    value.chars().count()
}

fn column_width<'a, I>(header: &str, min_width: usize, values: I) -> usize
where
    I: IntoIterator<Item = &'a str>,
{
    values
        .into_iter()
        .fold(text_width(header).max(min_width), |width, value| {
            width.max(text_width(value))
        })
}

fn installed_text_rows(rows: &[InstalledToolRow]) -> Vec<InstalledTextRow> {
    rows.iter()
        .map(|row| {
            let active = row
                .active_version
                .as_deref()
                .map(str::to_string)
                .unwrap_or_else(|| "-".to_string());
            let active = if row.active_missing_from_store {
                format!("{active} !")
            } else {
                active
            };

            InstalledTextRow {
                name: row.name.clone(),
                active,
                versions: row.installed_versions.len().to_string(),
                source: row.source.as_deref().unwrap_or("-").to_string(),
            }
        })
        .collect()
}

fn render_installed_table(rows: &[InstalledToolRow]) -> Option<String> {
    if rows.is_empty() {
        return None;
    }

    let display_rows = installed_text_rows(rows);
    let name_width = column_width("NAME", 24, display_rows.iter().map(|row| row.name.as_str()));
    let active_width = column_width(
        "ACTIVE",
        20,
        display_rows.iter().map(|row| row.active.as_str()),
    );
    let versions_width = column_width(
        "VERSIONS",
        8,
        display_rows.iter().map(|row| row.versions.as_str()),
    );

    let mut lines = Vec::with_capacity(display_rows.len() + 1);
    lines.push(format!(
        "{:<name_width$} {:<active_width$} {:<versions_width$} SOURCE",
        "NAME", "ACTIVE", "VERSIONS",
    ));
    for row in display_rows {
        lines.push(format!(
            "{:<name_width$} {:<active_width$} {:<versions_width$} {}",
            row.name, row.active, row.versions, row.source
        ));
    }
    Some(lines.join("\n"))
}

fn outdated_text_rows(rows: &[OutdatedRow]) -> Vec<OutdatedTextRow> {
    rows.iter()
        .map(|row| OutdatedTextRow {
            name: row.name.clone(),
            version: row.version.clone(),
            active: if row.active {
                "*".to_string()
            } else {
                String::new()
            },
            update: row.update.clone(),
            source: row.source.clone(),
        })
        .collect()
}

fn render_outdated_table(rows: &[OutdatedRow]) -> Option<String> {
    if rows.is_empty() {
        return None;
    }

    let display_rows = outdated_text_rows(rows);
    let name_width = column_width("NAME", 24, display_rows.iter().map(|row| row.name.as_str()));
    let version_width = column_width(
        "VERSION",
        20,
        display_rows.iter().map(|row| row.version.as_str()),
    );
    let active_width = column_width(
        "ACTIVE",
        6,
        display_rows.iter().map(|row| row.active.as_str()),
    );
    let update_width = column_width(
        "UPDATE",
        18,
        display_rows.iter().map(|row| row.update.as_str()),
    );

    let mut lines = Vec::with_capacity(display_rows.len() + 1);
    lines.push(format!(
        "{:<name_width$} {:<version_width$} {:<active_width$} {:<update_width$} SOURCE",
        "NAME", "VERSION", "ACTIVE", "UPDATE",
    ));
    for row in display_rows {
        lines.push(format!(
            "{:<name_width$} {:<version_width$} {:<active_width$} {:<update_width$} {}",
            row.name, row.version, row.active, row.update, row.source
        ));
    }
    Some(lines.join("\n"))
}

fn print_installed_text(report: &InstalledToolReport) {
    if let Some(table) = render_installed_table(&report.rows) {
        println!("{table}");
    } else {
        println!("No managed tools installed.");
    }

    println!("\nScope: {}", report.scope);
    println!("Tool binaries path: {}", report.bin_path);
    print_unmanaged_binaries_text(&report.unmanaged);
}

fn print_installed_json(report: &InstalledToolReport) -> Result<()> {
    let json = InstalledToolReportJson {
        scope: report.scope.clone(),
        tool_binaries_path: report.bin_path.clone(),
        rows: report
            .rows
            .iter()
            .map(|row| InstalledToolRowJson {
                name: row.name.clone(),
                active_version: row.active_version.clone(),
                active_missing_from_store: row.active_missing_from_store,
                installed_versions: row.installed_versions.clone(),
                installed_count: row.installed_versions.len(),
                source: row.source.clone(),
                bin_path: row.bin_path.clone(),
            })
            .collect(),
        unmanaged: report.unmanaged.clone(),
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&json).context("serialize tool list JSON")?
    );
    Ok(())
}

fn print_tool_detail_text(report: &ToolDetailReport) {
    println!("Tool: {}", report.name);
    println!("Scope: {}", report.scope);
    println!("Managed: {}", if report.managed { "yes" } else { "no" });

    let active = report
        .active_version
        .as_deref()
        .map(str::to_string)
        .unwrap_or_else(|| "-".to_string());
    if report.active_missing_from_store {
        println!("Active version: {active} (missing from store)");
    } else {
        println!("Active version: {active}");
    }
    println!(
        "Active path: {}",
        report.active_bin_path.as_deref().unwrap_or("-")
    );
    println!(
        "Supported source: {}",
        report
            .supported_source
            .as_deref()
            .unwrap_or("custom / unmanaged")
    );
    println!(
        "Aliases: {}",
        if report.aliases.is_empty() {
            "-".to_string()
        } else {
            report.aliases.join(", ")
        }
    );

    if report.installed.is_empty() {
        println!("Installed versions: none");
    } else {
        println!("Installed versions:");
        for item in &report.installed {
            let active_marker = if item.active { " [active]" } else { "" };
            println!(
                "- {}{}  source={}  path={}",
                item.version, active_marker, item.source, item.executable_path
            );
        }
    }

    if let Some(unmanaged) = &report.unmanaged {
        println!(
            "Unmanaged binary: {} {} ({})",
            unmanaged.name, unmanaged.version, unmanaged.path
        );
        println!(
            "Tip: run `za tool install {} --adopt` to manage it in this scope.",
            unmanaged.name
        );
    }
}

fn print_tool_detail_json(report: &ToolDetailReport) -> Result<()> {
    let json = ToolDetailReportJson {
        name: report.name.clone(),
        aliases: report.aliases.clone(),
        scope: report.scope.clone(),
        managed: report.managed,
        active_version: report.active_version.clone(),
        active_missing_from_store: report.active_missing_from_store,
        active_bin_path: report.active_bin_path.clone(),
        supported_source: report.supported_source.clone(),
        installed: report
            .installed
            .iter()
            .map(|item| ToolVersionDetailJson {
                version: item.version.clone(),
                active: item.active,
                source: item.source.clone(),
                executable_path: item.executable_path.clone(),
                manifest_path: item.manifest_path.clone(),
            })
            .collect(),
        unmanaged: report.unmanaged.clone(),
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&json).context("serialize tool detail JSON")?
    );
    Ok(())
}

fn print_outdated_text(report: &OutdatedReport) {
    if let Some(table) = render_outdated_table(&report.rows) {
        println!("{table}");
    } else {
        println!("No managed tools installed.");
    }

    println!("\nScope: {}", report.scope);
    println!("Tool binaries path: {}", report.bin_path);
    if !report.check_failures.is_empty() {
        println!("\nUpdate check warnings:");
        for (name, err) in &report.check_failures {
            println!("- {name}: {err}");
        }
    }
    print_unmanaged_binaries_text(&report.unmanaged);
}

fn print_outdated_json(report: &OutdatedReport) -> Result<()> {
    let json = OutdatedReportJson {
        scope: report.scope.clone(),
        tool_binaries_path: report.bin_path.clone(),
        rows: report
            .rows
            .iter()
            .map(|row| OutdatedRowJson {
                name: row.name.clone(),
                version: row.version.clone(),
                active: row.active,
                source: row.source.clone(),
                update: row.update.clone(),
                latest: row.latest.clone(),
            })
            .collect(),
        unmanaged: report.unmanaged.clone(),
        update_check_warnings: report
            .check_failures
            .iter()
            .map(|(name, err)| format!("{name}: {err}"))
            .collect(),
        has_updates: report.has_updates,
        has_check_failures: !report.check_failures.is_empty(),
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&json).context("serialize tool outdated JSON")?
    );
    Ok(())
}

fn supported_tools_view() -> Vec<SupportedToolView> {
    tool_policies()
        .iter()
        .map(|policy| SupportedToolView {
            tool: policy.canonical_name.to_string(),
            aliases: policy
                .aliases
                .iter()
                .map(|alias| (*alias).to_string())
                .collect(),
            sources: policy.source_label.to_string(),
        })
        .collect()
}

fn print_supported_tools(rows: &[SupportedToolView]) {
    println!("{:<24} SOURCES", "TOOL");
    for row in rows {
        let tool_display = if row.aliases.is_empty() {
            row.tool.clone()
        } else {
            format!("{} / {}", row.tool, row.aliases.join(" / "))
        };
        println!("{:<24} {}", tool_display, row.sources);
    }
}

fn print_unmanaged_binaries_text(unmanaged: &[UnmanagedBinary]) {
    if unmanaged.is_empty() {
        return;
    }
    for item in unmanaged {
        println!(
            "\nDetected unmanaged binary: {} {} ({})",
            item.name, item.version, item.path
        );
        println!(
            "Run `za tool install {} --adopt` to move it into the managed store.",
            item.name
        );
    }
}

fn resolve_latest_checks_for_names(names: &[String]) -> Result<LatestLookup> {
    let mut latest_by_name: HashMap<String, LatestCheck> = HashMap::new();
    let mut policy_tasks = Vec::new();
    let mut policy_seen: HashMap<&'static str, ()> = HashMap::new();

    let mut cache = ToolUpdateCacheState::load();
    let now_unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    for name in names {
        let Some(policy) = find_tool_policy(name) else {
            latest_by_name.insert(name.clone(), LatestCheck::Unsupported);
            continue;
        };
        if policy_seen.contains_key(policy.canonical_name) {
            continue;
        }
        policy_seen.insert(policy.canonical_name, ());
        if let Some(latest) = cache.get_latest_if_fresh(policy.canonical_name, now_unix_secs) {
            latest_by_name.insert(
                policy.canonical_name.to_string(),
                LatestCheck::Latest(latest),
            );
        } else {
            policy_tasks.push(policy);
        }
    }

    if !policy_tasks.is_empty() {
        let fetched = fetch_latest_checks_parallel(policy_tasks);
        for (canonical_name, latest_check) in fetched {
            if let LatestCheck::Latest(version) = &latest_check {
                cache.put_latest(canonical_name, version.clone(), now_unix_secs);
            }
            latest_by_name.insert(canonical_name.to_string(), latest_check);
        }
    }

    if let Err(err) = cache.save_if_dirty()
        && !is_permission_denied_error(&err)
    {
        eprintln!("warning: failed to persist tool update cache: {err}");
    }

    let mut by_name = HashMap::new();
    for name in names {
        let Some(policy) = find_tool_policy(name) else {
            by_name.insert(name.clone(), LatestCheck::Unsupported);
            continue;
        };
        let latest = latest_by_name
            .get(policy.canonical_name)
            .cloned()
            .unwrap_or(LatestCheck::Unsupported);
        by_name.insert(name.clone(), latest);
    }

    Ok(LatestLookup {
        latest_by_name: by_name,
    })
}

fn fetch_latest_checks_parallel(policies: Vec<ToolPolicy>) -> HashMap<&'static str, LatestCheck> {
    let worker_count = normalize_tool_update_jobs(default_tool_update_jobs(), policies.len());
    let queue = Arc::new(Mutex::new(VecDeque::from(policies)));
    let out: Arc<Mutex<HashMap<&'static str, LatestCheck>>> = Arc::new(Mutex::new(HashMap::new()));
    let progress = new_tool_progress_bar(
        "update",
        queue.lock().map(|guard| guard.len()).unwrap_or(0),
        "checking upstream releases",
    );

    thread::scope(|scope| {
        for _ in 0..worker_count {
            let queue = Arc::clone(&queue);
            let out = Arc::clone(&out);
            let progress = progress.clone();
            scope.spawn(move || {
                loop {
                    let task = match queue.lock() {
                        Ok(mut guard) => guard.pop_front(),
                        Err(_) => None,
                    };
                    let Some(policy) = task else {
                        break;
                    };
                    let latest = resolve_latest_for_policy(policy);
                    if let Some(progress) = progress.as_ref() {
                        progress.set_message(latest_check_progress_message(policy, &latest));
                        progress.inc(1);
                    }
                    if let Ok(mut guard) = out.lock() {
                        guard.insert(policy.canonical_name, latest);
                    } else {
                        break;
                    }
                }
            });
        }
    });

    if let Some(progress) = progress {
        progress.finish_and_clear();
    }

    out.lock()
        .map(|guard| guard.clone())
        .unwrap_or_else(|_| HashMap::new())
}

fn resolve_latest_for_policy(policy: ToolPolicy) -> LatestCheck {
    let Some(release) = policy.github_release else {
        return LatestCheck::Unsupported;
    };
    match source::fetch_latest_version_from_github_release(
        policy,
        release,
        za_config::ProxyScope::Tool,
    ) {
        Ok(version) => LatestCheck::Latest(version),
        Err(err) => LatestCheck::Error(format!("{err:#}")),
    }
}

fn normalize_tool_update_jobs(requested_jobs: usize, task_count: usize) -> usize {
    requested_jobs.max(1).min(task_count.max(1))
}

fn default_tool_update_jobs() -> usize {
    let cpus = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(TOOL_UPDATE_JOBS_MIN);
    cpus.saturating_mul(TOOL_UPDATE_JOBS_MULTIPLIER)
        .clamp(TOOL_UPDATE_JOBS_MIN, TOOL_UPDATE_JOBS_MAX)
}

fn tool_update_cache_path() -> Option<PathBuf> {
    if let Some(path) = env::var_os("XDG_CACHE_HOME").map(PathBuf::from) {
        return Some(path.join("za").join(TOOL_UPDATE_CACHE_FILE_NAME));
    }
    env::var_os("HOME").map(PathBuf::from).map(|home| {
        home.join(".cache")
            .join("za")
            .join(TOOL_UPDATE_CACHE_FILE_NAME)
    })
}

pub(super) fn latest_check_progress_message(policy: ToolPolicy, latest: &LatestCheck) -> String {
    match latest {
        LatestCheck::Latest(version) => format!("{} {}", policy.canonical_name, version),
        LatestCheck::Unsupported => format!("{} n/a", policy.canonical_name),
        LatestCheck::Error(_) => format!("{} failed", policy.canonical_name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_installed_table_expands_active_column_for_long_versions() {
        let rows = vec![
            InstalledToolRow {
                name: "ble.sh".to_string(),
                active_version: Some("nightly-20260310+b99cadb".to_string()),
                active_missing_from_store: false,
                installed_versions: vec!["nightly-20260310+b99cadb".to_string()],
                source: Some("download".to_string()),
                bin_path: None,
            },
            InstalledToolRow {
                name: "za".to_string(),
                active_version: Some("0.1.41".to_string()),
                active_missing_from_store: false,
                installed_versions: vec!["0.1.41".to_string()],
                source: Some("download".to_string()),
                bin_path: None,
            },
        ];

        let expected = [
            format!("{:<24} {:<24} {:<8} SOURCE", "NAME", "ACTIVE", "VERSIONS"),
            format!(
                "{:<24} {:<24} {:<8} {}",
                "ble.sh", "nightly-20260310+b99cadb", "1", "download"
            ),
            format!("{:<24} {:<24} {:<8} {}", "za", "0.1.41", "1", "download"),
        ]
        .join("\n");

        assert_eq!(render_installed_table(&rows), Some(expected));
    }

    #[test]
    fn render_outdated_table_expands_update_column_for_long_versions() {
        let rows = vec![
            OutdatedRow {
                name: "ble.sh".to_string(),
                version: "nightly-20260310+b99cadb".to_string(),
                active: true,
                source: "download".to_string(),
                update: "update -> nightly-20260317+cafebabe".to_string(),
                latest: Some("nightly-20260317+cafebabe".to_string()),
            },
            OutdatedRow {
                name: "za".to_string(),
                version: "0.1.41".to_string(),
                active: false,
                source: "download".to_string(),
                update: "latest".to_string(),
                latest: Some("0.1.41".to_string()),
            },
        ];

        let expected = [
            format!(
                "{:<24} {:<24} {:<6} {:<35} SOURCE",
                "NAME", "VERSION", "ACTIVE", "UPDATE"
            ),
            format!(
                "{:<24} {:<24} {:<6} {:<35} {}",
                "ble.sh",
                "nightly-20260310+b99cadb",
                "*",
                "update -> nightly-20260317+cafebabe",
                "download"
            ),
            format!(
                "{:<24} {:<24} {:<6} {:<35} {}",
                "za", "0.1.41", "", "latest", "download"
            ),
        ]
        .join("\n");

        assert_eq!(render_outdated_table(&rows), Some(expected));
    }
}
