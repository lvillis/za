use super::latest::{
    LatestQuerySource, LatestRecord, LatestStatus, LatestSuggestionKind, LatestSummary,
};
use super::*;

pub(super) fn build_summary(records: &[DepAuditRecord]) -> AuditSummary {
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

pub(super) fn print_report(
    manifest_path: &Path,
    summary: &AuditSummary,
    records: &[DepAuditRecord],
    verbose: bool,
) {
    for line in render_report_lines(manifest_path, summary, records, verbose) {
        println!("{line}");
    }
}

pub(super) fn render_report_lines(
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
        text_render::join_dim_bullets(&parts)
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
        let name = style_dep_name_cell(
            &text_render::truncate_end(&record.name, name_width),
            name_width,
        );
        let requirement = tty_style::dim(format!(
            "{:<req_width$}",
            text_render::truncate_end(&record.requirement, req_width)
        ));
        let latest = style_dep_latest_cell(
            &text_render::truncate_end(
                record.latest_version.as_deref().unwrap_or("-"),
                latest_width,
            ),
            latest_width,
            record.latest_version.is_some(),
        );
        let kinds = tty_style::dim(format!(
            "{:<kinds_width$}",
            text_render::truncate_end(&record.kinds, kinds_width)
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
    text_render::truncate_end(&record.notes.join("; "), 96)
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

pub(super) fn write_json_report(
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

pub(super) fn render_latest_lines(
    manifest_path: Option<&Path>,
    summary: &LatestSummary,
    records: &[LatestRecord],
    suggest: bool,
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
    let plan_width = records
        .iter()
        .map(|record| {
            record
                .suggestion_kind
                .map(latest_suggestion_label)
                .unwrap_or("-")
                .chars()
                .count()
        })
        .max()
        .unwrap_or(6)
        .clamp(4, 8);
    let suggest_width = records
        .iter()
        .map(|record| {
            record
                .suggested_requirement
                .as_deref()
                .unwrap_or("-")
                .chars()
                .count()
        })
        .max()
        .unwrap_or(7)
        .clamp(7, 20);

    lines.push(String::new());
    if suggest {
        lines.push(tty_style::dim(format!(
            "{:<5}  {:<name_width$}  {:<req_width$}  {:<latest_width$}  {:<plan_width$}  {:<suggest_width$}  note",
            "st", "name", "req", "latest", "plan", "suggest"
        )));
        for record in records {
            lines.push(format!(
                "{}  {:<name_width$}  {:<req_width$}  {:<latest_width$}  {}  {}  {}",
                style_latest_status(record.status),
                text_render::truncate_end(&record.name, name_width),
                text_render::truncate_end(record.requirement.as_deref().unwrap_or("-"), req_width),
                style_latest_version_cell(
                    &text_render::truncate_end(
                        record.latest_version.as_deref().unwrap_or("-"),
                        latest_width,
                    ),
                    latest_width,
                    record.status,
                ),
                style_latest_plan_cell(record.suggestion_kind, plan_width),
                style_latest_suggest_cell(
                    &text_render::truncate_end(
                        record.suggested_requirement.as_deref().unwrap_or("-"),
                        suggest_width,
                    ),
                    suggest_width,
                    record.suggestion_kind,
                ),
                text_render::truncate_end(&render_latest_note(record, true), 96)
            ));
        }
    } else {
        lines.push(tty_style::dim(format!(
            "{:<5}  {:<name_width$}  {:<req_width$}  {:<latest_width$}  {:<kinds_width$}  note",
            "st", "name", "req", "latest", "kinds"
        )));
        for record in records {
            lines.push(format!(
                "{}  {:<name_width$}  {:<req_width$}  {:<latest_width$}  {:<kinds_width$}  {}",
                style_latest_status(record.status),
                text_render::truncate_end(&record.name, name_width),
                text_render::truncate_end(record.requirement.as_deref().unwrap_or("-"), req_width),
                style_latest_version_cell(
                    &text_render::truncate_end(
                        record.latest_version.as_deref().unwrap_or("-"),
                        latest_width,
                    ),
                    latest_width,
                    record.status,
                ),
                tty_style::dim(format!(
                    "{:<kinds_width$}",
                    text_render::truncate_end(record.kinds.as_deref().unwrap_or("-"), kinds_width)
                )),
                text_render::truncate_end(&render_latest_note(record, false), 96)
            ));
        }
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

pub(super) fn render_latest_toml(records: &[LatestRecord]) -> String {
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

fn render_latest_note(record: &LatestRecord, suggest: bool) -> String {
    let mut parts = Vec::new();
    if suggest && let Some(note) = record.suggestion_note.as_deref() {
        parts.push(note.to_string());
    }
    if let Some(note) = record.note.as_deref() {
        parts.push(note.to_string());
    }
    if parts.is_empty() {
        match record.source {
            LatestQuerySource::Args => "explicit query".to_string(),
            LatestQuerySource::Manifest => "manifest".to_string(),
        }
    } else {
        parts.join("; ")
    }
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
        text_render::join_dim_bullets(&parts)
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

fn latest_suggestion_label(kind: LatestSuggestionKind) -> &'static str {
    match kind {
        LatestSuggestionKind::Add => "add",
        LatestSuggestionKind::Keep => "keep",
        LatestSuggestionKind::Bump => "bump",
        LatestSuggestionKind::Review => "review",
    }
}

fn style_latest_plan_cell(kind: Option<LatestSuggestionKind>, width: usize) -> String {
    let padded = format!(
        "{:<width$}",
        kind.map(latest_suggestion_label).unwrap_or("-")
    );
    match kind {
        Some(LatestSuggestionKind::Add | LatestSuggestionKind::Bump) => tty_style::active(padded),
        Some(LatestSuggestionKind::Review) => tty_style::warning(padded),
        Some(LatestSuggestionKind::Keep) => tty_style::dim(padded),
        None => tty_style::dim(padded),
    }
}

fn style_latest_suggest_cell(
    value: &str,
    width: usize,
    kind: Option<LatestSuggestionKind>,
) -> String {
    let padded = format!("{value:<width$}");
    match kind {
        Some(LatestSuggestionKind::Add | LatestSuggestionKind::Bump) => tty_style::active(padded),
        Some(LatestSuggestionKind::Review) => tty_style::warning(padded),
        Some(LatestSuggestionKind::Keep) | None => tty_style::dim(padded),
    }
}
