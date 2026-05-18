use super::latest::{
    LatestQuerySource, LatestRecord, LatestStatus, LatestSuggestionKind, LatestSummary,
};
use super::model::DependencyUpdatePlan;
use super::*;

pub(super) fn build_summary(records: &[DepAuditRecord], skipped_local: usize) -> AuditSummary {
    let mut summary = AuditSummary::default();
    for rec in records {
        match rec.risk {
            RiskLevel::High => summary.high += 1,
            RiskLevel::Medium => summary.medium += 1,
            RiskLevel::Low => summary.low += 1,
            RiskLevel::Unknown => summary.unknown += 1,
        }
    }
    summary.skipped_local = skipped_local;
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
        records,
        records.len(),
    )];
    let attention = records
        .iter()
        .filter(|record| record_needs_attention(record))
        .collect::<Vec<_>>();
    let baseline = records
        .iter()
        .filter(|record| !record_needs_attention(record))
        .collect::<Vec<_>>();

    if !attention.is_empty() {
        lines.push(String::new());
        lines.push(tty_style::header("attention"));
        lines.extend(render_record_table(&attention));
    }

    if verbose {
        if !baseline.is_empty() {
            lines.push(String::new());
            lines.push(tty_style::header("baseline"));
            lines.extend(render_record_table(&baseline));
        }
        lines.push(String::new());
        lines.push(format!(
            "{}  {}",
            tty_style::dim("manifest"),
            manifest_path.display()
        ));
    } else if !baseline.is_empty() {
        lines.push(String::new());
        lines.push(format!(
            "{} baseline {} hidden; use `--verbose` to show all",
            baseline.len(),
            entry_label(baseline.len()),
        ));
    }

    lines
}

fn plural_suffix(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

fn entry_label(count: usize) -> &'static str {
    if count == 1 { "entry" } else { "entries" }
}

fn count_label(count: usize, noun: &str) -> String {
    format!("{} {}{}", count, noun, plural_suffix(count))
}

fn render_report_summary_line(
    manifest_path: &Path,
    summary: &AuditSummary,
    records: &[DepAuditRecord],
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
        render_summary_counts(summary, records)
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

fn render_summary_counts(summary: &AuditSummary, records: &[DepAuditRecord]) -> String {
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
    if summary.skipped_local > 0 {
        parts.push(tty_style::dim(format!(
            "{} internal skipped",
            summary.skipped_local
        )));
    }
    let (bump_count, review_count) = version_attention_counts(records);
    if bump_count > 0 {
        parts.push(tty_style::active(count_label(bump_count, "update")));
    }
    if review_count > 0 {
        parts.push(tty_style::warning(count_label(review_count, "review")));
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
    let show_plan = records.iter().any(|record| {
        record
            .update_plan
            .is_some_and(DependencyUpdatePlan::needs_attention)
    });
    let plan_width = if show_plan {
        records
            .iter()
            .filter(|record| {
                record
                    .update_plan
                    .is_some_and(DependencyUpdatePlan::needs_attention)
            })
            .map(|record| update_plan_label(record.update_plan).chars().count())
            .max()
            .unwrap_or(4)
            .clamp(4, 8)
    } else {
        0
    };
    let kinds_width = column_width(records, "kinds", |record| &record.kinds, 12);

    let mut lines = Vec::with_capacity(records.len() + 1);
    if show_plan {
        lines.push(tty_style::dim(format!(
            "{:<5}  {:<name_width$}  {:<req_width$}  {:<latest_width$}  {:<plan_width$}  {:<kinds_width$}  note",
            "risk", "name", "req", "latest", "plan", "kinds",
        )));
    } else {
        lines.push(tty_style::dim(format!(
            "{:<5}  {:<name_width$}  {:<req_width$}  {:<latest_width$}  {:<kinds_width$}  note",
            "risk", "name", "req", "latest", "kinds",
        )));
    }

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
        if show_plan {
            let plan = style_update_plan_cell(
                record.update_plan.filter(|plan| plan.needs_attention()),
                plan_width,
            );
            lines.push(format!(
                "{}  {}  {}  {}  {}  {}  {}",
                risk,
                name,
                requirement,
                latest,
                plan,
                kinds,
                summarize_record_note(record),
            ));
        } else {
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

fn record_needs_attention(record: &DepAuditRecord) -> bool {
    record.risk != RiskLevel::Low
        || record
            .update_plan
            .is_some_and(DependencyUpdatePlan::needs_attention)
}

fn version_attention_counts(records: &[DepAuditRecord]) -> (usize, usize) {
    let mut bump_count = 0;
    let mut review_count = 0;
    for record in records {
        match record.update_plan {
            Some(DependencyUpdatePlan::Add | DependencyUpdatePlan::Bump) => bump_count += 1,
            Some(DependencyUpdatePlan::Review) => review_count += 1,
            Some(DependencyUpdatePlan::Keep) | None => {}
        }
    }
    (bump_count, review_count)
}

fn summarize_record_note(record: &DepAuditRecord) -> String {
    let mut notes = Vec::<String>::new();
    if record
        .update_plan
        .is_some_and(DependencyUpdatePlan::needs_attention)
        && let Some(note) = record.update_note.as_deref()
    {
        push_compact_note(&mut notes, compact_update_note(note));
    }
    for note in &record.notes {
        push_compact_note(&mut notes, compact_record_note(note));
    }

    if notes.is_empty() {
        return tty_style::dim("-");
    }
    let extra = notes.len().saturating_sub(4);
    let mut displayed = notes.into_iter().take(4).collect::<Vec<_>>();
    if extra > 0 {
        displayed.push(format!("+{extra}"));
    }
    displayed.join(",")
}

fn push_compact_note(notes: &mut Vec<String>, note: impl Into<String>) {
    let note = note.into();
    if !notes.iter().any(|existing| existing == &note) {
        notes.push(note);
    }
}

fn compact_update_note(note: &str) -> String {
    match note {
        "latest version format needs manual review" => "bad-latest".to_string(),
        "dependency has no explicit manifest requirement" => "no-req".to_string(),
        "complex manifest requirement; review manually" => "complex-req".to_string(),
        "manifest requirement is not a plain semver range" => "non-semver-req".to_string(),
        "same release line; refresh manifest requirement" => "same-line".to_string(),
        "major or nontrivial upgrade; review compatibility" => "major".to_string(),
        _ => text_render::truncate_end(note, 24),
    }
}

fn compact_record_note(note: &str) -> String {
    if note == "latest published crate version is yanked" {
        return "yanked".to_string();
    }
    if note == "license metadata missing on latest crate release" {
        return "no-license".to_string();
    }
    if note == "MSRV not declared on latest crate release" {
        return "no-msrv".to_string();
    }
    if note == "GitHub repo is archived" {
        return "archived".to_string();
    }
    if note == "repository is not a GitHub repo URL" {
        return "non-github".to_string();
    }
    if note == "repository URL missing" {
        return "no-repo".to_string();
    }
    if note == "insufficient maintenance signals" {
        return "no-signals".to_string();
    }
    if note.starts_with("GitHub signals unavailable") {
        return "github?".to_string();
    }
    if note.starts_with("crates.io query failed:") {
        return "crates.io?".to_string();
    }
    if note.starts_with("GitHub query failed:") {
        return "github?".to_string();
    }
    if let Some(days) = extract_days(note, "latest crate release is stale (") {
        return format!("release-stale:{days}d");
    }
    if let Some(days) = extract_days(note, "crate release not recent (") {
        return format!("release-old:{days}d");
    }
    if let Some(days) = extract_days(note, "GitHub repo activity is stale (") {
        return format!("repo-stale:{days}d");
    }
    if let Some(days) = extract_days(note, "GitHub activity older than 1 year (") {
        return format!("repo-old:{days}d");
    }
    if let Some(stars) = extract_parenthesized_value(note, "low community signal (stars=") {
        return format!("low-stars:{stars}");
    }
    if let Some(stars) = extract_parenthesized_value(note, "small community size (stars=") {
        return format!("small-stars:{stars}");
    }
    if let Some(std_alt) = note.strip_prefix("std alternative available: ") {
        return format!("std:{std_alt}");
    }
    text_render::truncate_end(note, 24)
}

fn extract_days(note: &str, prefix: &str) -> Option<String> {
    extract_parenthesized_value(note, prefix)
        .map(|value| value.trim_end_matches(" days").to_string())
}

fn extract_parenthesized_value(note: &str, prefix: &str) -> Option<String> {
    note.strip_prefix(prefix)
        .and_then(|rest| rest.strip_suffix(')'))
        .map(str::to_string)
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

fn update_plan_label(plan: Option<DependencyUpdatePlan>) -> &'static str {
    match plan {
        Some(DependencyUpdatePlan::Add) => "add",
        Some(DependencyUpdatePlan::Keep) => "keep",
        Some(DependencyUpdatePlan::Bump) => "bump",
        Some(DependencyUpdatePlan::Review) => "review",
        None => "-",
    }
}

fn style_update_plan_cell(plan: Option<DependencyUpdatePlan>, width: usize) -> String {
    let padded = format!("{:<width$}", update_plan_label(plan));
    match plan {
        Some(DependencyUpdatePlan::Add | DependencyUpdatePlan::Bump) => tty_style::active(padded),
        Some(DependencyUpdatePlan::Review) => tty_style::warning(padded),
        Some(DependencyUpdatePlan::Keep) | None => tty_style::dim(padded),
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
    write_file_atomically(&path, json).with_context(|| format!("write {}", path.display()))?;
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
