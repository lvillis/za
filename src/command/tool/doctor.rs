use super::*;
use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum DoctorStatus {
    Ok,
    Warn,
    Error,
}

impl DoctorStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Warn => "WARN",
            Self::Error => "ERR",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DoctorIssueSeverity {
    Warn,
    Error,
}

#[derive(Debug, Clone, Serialize)]
struct ToolDoctorRow {
    name: String,
    active_version: Option<String>,
    status: DoctorStatus,
    installed_versions: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    issues: Vec<String>,
    current_file: String,
    active_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    install_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    manifest_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
struct ToolDoctorSummary {
    ok: usize,
    warn: usize,
    error: usize,
}

#[derive(Debug, Clone, Serialize)]
struct ToolDoctorReport {
    scope: String,
    store_dir: String,
    current_dir: String,
    bin_dir: String,
    summary: ToolDoctorSummary,
    rows: Vec<ToolDoctorRow>,
}

pub(super) fn run_doctor(home: &ToolHome, tools: &[String], json: bool) -> Result<i32> {
    let names = doctor_target_names(home, tools)?;
    let mut rows = Vec::with_capacity(names.len());
    let mut summary = ToolDoctorSummary::default();

    for name in names {
        let row = inspect_tool(home, &name)?;
        match row.status {
            DoctorStatus::Ok => summary.ok += 1,
            DoctorStatus::Warn => summary.warn += 1,
            DoctorStatus::Error => summary.error += 1,
        }
        rows.push(row);
    }

    rows.sort_by(|a, b| {
        doctor_sort_weight(a.status)
            .cmp(&doctor_sort_weight(b.status))
            .then_with(|| a.name.cmp(&b.name))
    });

    let report = ToolDoctorReport {
        scope: home.scope.label().to_string(),
        store_dir: home.store_dir.display().to_string(),
        current_dir: home.current_dir.display().to_string(),
        bin_dir: home.bin_dir.display().to_string(),
        summary,
        rows,
    };

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).context("serialize tool doctor output")?
        );
    } else {
        for line in render_doctor_lines(&report) {
            println!("{line}");
        }
    }

    Ok(0)
}

fn doctor_target_names(home: &ToolHome, tools: &[String]) -> Result<Vec<String>> {
    if tools.is_empty() {
        return collect_managed_tool_names(home);
    }
    let mut names = tools
        .iter()
        .map(|tool| {
            let spec = ToolSpec::from_args(tool, None)?;
            Ok(canonical_tool_name(&spec.name))
        })
        .collect::<Result<Vec<_>>>()?;
    names.sort();
    names.dedup();
    Ok(names)
}

fn inspect_tool(home: &ToolHome, name: &str) -> Result<ToolDoctorRow> {
    let current_file = home.current_file(name);
    let active_path = home.active_path(name);
    let current_file_display = current_file.display().to_string();
    let active_path_display = active_path.display().to_string();
    let installed_versions = collect_dir_names(&home.name_dir(name))?.len();
    let mut issues = Vec::new();
    let active_version = match read_current_version(home, name) {
        Ok(version) => version,
        Err(err) => {
            issues.push((
                DoctorIssueSeverity::Error,
                format!("current version unreadable: {err}"),
            ));
            None
        }
    };

    let mut install_path = None::<String>;
    let mut manifest_path = None::<String>;

    match active_version.as_deref() {
        Some(version) => {
            let tool = ToolRef {
                name: name.to_string(),
                version: version.to_string(),
            };
            let is_package = package_policy_for_name(name).is_some();
            let version_dir = home.version_dir(&tool);
            let payload_path = home.install_path(&tool);
            let tool_manifest = home.manifest_path(&tool);
            install_path = Some(payload_path.display().to_string());
            manifest_path = Some(tool_manifest.display().to_string());

            if !version_dir.exists() {
                issues.push((
                    DoctorIssueSeverity::Error,
                    format!(
                        "active version directory missing: {}; repair with `za tool update {name}`",
                        version_dir.display()
                    ),
                ));
            }
            if !payload_path.exists() {
                issues.push((
                    DoctorIssueSeverity::Error,
                    format!(
                        "installed payload missing: {}; repair with `za tool update {name}`",
                        payload_path.display()
                    ),
                ));
            } else if !is_package && !is_executable_file(&payload_path) {
                issues.push((
                    DoctorIssueSeverity::Error,
                    format!(
                        "installed payload is not executable: {}",
                        payload_path.display()
                    ),
                ));
            }
            if !active_path.exists() {
                issues.push((
                    DoctorIssueSeverity::Error,
                    format!(
                        "active path missing: {}; repair with `za tool update {name}`",
                        active_path.display()
                    ),
                ));
            } else if !is_package && !is_executable_file(&active_path) {
                issues.push((
                    DoctorIssueSeverity::Error,
                    format!("active path is not executable: {}", active_path.display()),
                ));
            }
            inspect_manifest(home, &tool, &mut issues)?;
        }
        None => {
            if installed_versions > 0 {
                issues.push((
                    DoctorIssueSeverity::Warn,
                    "versions are installed but no active version is selected".to_string(),
                ));
            } else if active_path.exists() {
                issues.push((
                    DoctorIssueSeverity::Warn,
                    format!(
                        "active path exists without any managed versions: {}",
                        active_path.display()
                    ),
                ));
            }
        }
    }

    let status = issues
        .iter()
        .fold(DoctorStatus::Ok, |status, (severity, _)| match severity {
            DoctorIssueSeverity::Warn if status == DoctorStatus::Ok => DoctorStatus::Warn,
            DoctorIssueSeverity::Warn => status,
            DoctorIssueSeverity::Error => DoctorStatus::Error,
        });

    Ok(ToolDoctorRow {
        name: name.to_string(),
        active_version,
        status,
        installed_versions,
        issues: issues.into_iter().map(|(_, message)| message).collect(),
        current_file: current_file_display,
        active_path: active_path_display,
        install_path,
        manifest_path,
    })
}

fn inspect_manifest(
    home: &ToolHome,
    tool: &ToolRef,
    issues: &mut Vec<(DoctorIssueSeverity, String)>,
) -> Result<()> {
    let manifest_path = home.manifest_path(tool);
    if !manifest_path.exists() {
        issues.push((
            DoctorIssueSeverity::Warn,
            format!("manifest missing: {}", manifest_path.display()),
        ));
        return Ok(());
    }

    let raw = match fs::read_to_string(&manifest_path) {
        Ok(raw) => raw,
        Err(err) => {
            issues.push((
                DoctorIssueSeverity::Error,
                format!("manifest unreadable: {} ({err})", manifest_path.display()),
            ));
            return Ok(());
        }
    };
    let manifest = serde_json::from_str::<ToolManifest>(&raw);

    match manifest {
        Ok(manifest) => {
            if manifest.name != tool.name {
                issues.push((
                    DoctorIssueSeverity::Error,
                    format!(
                        "manifest name mismatch: expected `{}`, found `{}`",
                        tool.name, manifest.name
                    ),
                ));
            }
            if normalize_version(&manifest.version) != normalize_version(&tool.version) {
                issues.push((
                    DoctorIssueSeverity::Error,
                    format!(
                        "manifest version mismatch: expected `{}`, found `{}`",
                        tool.version, manifest.version
                    ),
                ));
            }
        }
        Err(err) => issues.push((
            DoctorIssueSeverity::Error,
            format!("manifest invalid: {} ({err})", manifest_path.display()),
        )),
    }

    Ok(())
}

fn doctor_sort_weight(status: DoctorStatus) -> u8 {
    match status {
        DoctorStatus::Error => 0,
        DoctorStatus::Warn => 1,
        DoctorStatus::Ok => 2,
    }
}

fn render_doctor_lines(report: &ToolDoctorReport) -> Vec<String> {
    let mut lines = vec![format!(
        "{} {}  {}",
        style_doctor_status(if report.summary.error > 0 {
            DoctorStatus::Error
        } else if report.summary.warn > 0 {
            DoctorStatus::Warn
        } else {
            DoctorStatus::Ok
        }),
        tty_style::header("tool doctor"),
        render_doctor_summary(&report.summary)
    )];

    lines.push(format!(
        "{} {}  {} {}  {} {}",
        tty_style::dim("scope"),
        report.scope,
        tty_style::dim("store"),
        report.store_dir,
        tty_style::dim("bin"),
        report.bin_dir
    ));

    if report.rows.is_empty() {
        lines.push(tty_style::dim("No managed tools found in this scope."));
        return lines;
    }

    let tool_width = report
        .rows
        .iter()
        .map(|row| row.name.chars().count())
        .max()
        .unwrap_or(4)
        .clamp(4, 24);
    let version_width = report
        .rows
        .iter()
        .map(|row| row.active_version.as_deref().unwrap_or("-").chars().count())
        .max()
        .unwrap_or(6)
        .clamp(6, 24);

    lines.push(String::new());
    lines.push(tty_style::dim(format!(
        "{:<5}  {:<tool_width$}  {:<version_width$}  {:>5}  issues",
        "st", "tool", "active", "vers"
    )));
    for row in &report.rows {
        let issues = if row.issues.is_empty() {
            tty_style::dim("-")
        } else {
            truncate_doctor_issue(&row.issues.join("; "), 120)
        };
        lines.push(format!(
            "{}  {:<tool_width$}  {:<version_width$}  {:>5}  {}",
            style_doctor_status(row.status),
            row.name,
            row.active_version.as_deref().unwrap_or("-"),
            row.installed_versions,
            issues
        ));
    }

    lines
}

fn render_doctor_summary(summary: &ToolDoctorSummary) -> String {
    let mut parts = Vec::new();
    if summary.error > 0 {
        parts.push(tty_style::error(format!("{} error", summary.error)));
    }
    if summary.warn > 0 {
        parts.push(tty_style::warning(format!("{} warn", summary.warn)));
    }
    if summary.ok > 0 {
        parts.push(tty_style::success(format!("{} healthy", summary.ok)));
    }
    if parts.is_empty() {
        tty_style::dim("no managed tools")
    } else {
        parts.join(&format!(" {} ", tty_style::dim("·")))
    }
}

fn style_doctor_status(status: DoctorStatus) -> String {
    let label = format!("{:<5}", status.label());
    match status {
        DoctorStatus::Ok => tty_style::success(label),
        DoctorStatus::Warn => tty_style::warning(label),
        DoctorStatus::Error => tty_style::error(label),
    }
}

fn truncate_doctor_issue(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let mut out = String::new();
    for c in value.chars().take(max.saturating_sub(1)) {
        out.push(c);
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_doctor_summary_mentions_error_warn_and_ok_counts() {
        let summary = ToolDoctorSummary {
            ok: 3,
            warn: 1,
            error: 2,
        };
        let rendered = render_doctor_summary(&summary);
        assert!(rendered.contains("2 error"));
        assert!(rendered.contains("1 warn"));
        assert!(rendered.contains("3 healthy"));
    }

    #[test]
    fn truncate_doctor_issue_truncates_long_messages() {
        let rendered = truncate_doctor_issue("abcdefghijklmnopqrstuvwxyz", 8);
        assert_eq!(rendered, "abcdefg…");
    }
}
