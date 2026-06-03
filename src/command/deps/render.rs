use super::latest::{
    LatestQuerySource, LatestRecord, LatestStatus, LatestSuggestionKind, LatestSummary,
};
use super::model::{ActionUpdatePlan, DependencyUpdatePlan};
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
    actions: &[ActionAuditRecord],
    verbose: bool,
) {
    for line in render_report_lines(manifest_path, summary, records, actions, verbose) {
        println!("{line}");
    }
}

pub(super) fn render_report_lines(
    manifest_path: &Path,
    summary: &AuditSummary,
    records: &[DepAuditRecord],
    actions: &[ActionAuditRecord],
    verbose: bool,
) -> Vec<String> {
    let dep_updates = records
        .iter()
        .filter(|record| record_is_version_update(record))
        .collect::<Vec<_>>();
    let action_updates = actions
        .iter()
        .filter(|record| record.update_plan == ActionUpdatePlan::Bump)
        .collect::<Vec<_>>();
    let attention = records
        .iter()
        .filter(|record| record_needs_attention(record) && !record_is_version_update(record))
        .collect::<Vec<_>>();
    let baseline = records
        .iter()
        .filter(|record| !record_needs_attention(record) && !record_is_version_update(record))
        .collect::<Vec<_>>();
    let action_reviews = actions
        .iter()
        .filter(|record| record.update_plan == ActionUpdatePlan::Review)
        .collect::<Vec<_>>();
    let action_baseline = actions
        .iter()
        .filter(|record| !action_needs_attention(record))
        .collect::<Vec<_>>();

    let mut lines = render_report_summary_lines(
        manifest_path,
        summary,
        records,
        actions,
        baseline.len(),
        action_baseline.len(),
    );

    if !dep_updates.is_empty() || !action_updates.is_empty() {
        lines.push(String::new());
        lines.push(tty_style::header("updates"));
        lines.extend(render_update_table(
            manifest_path,
            &dep_updates,
            &action_updates,
        ));
    }

    if !attention.is_empty() {
        lines.push(String::new());
        lines.push(tty_style::header("attention"));
        lines.extend(render_record_table(&attention));
    }
    if !action_reviews.is_empty() {
        lines.push(String::new());
        lines.push(tty_style::header("action review"));
        lines.extend(render_action_table(&action_reviews));
    }

    if verbose {
        if !baseline.is_empty() {
            lines.push(String::new());
            lines.push(tty_style::header("deps baseline"));
            lines.extend(render_record_table(&baseline));
        }
        if !action_baseline.is_empty() {
            lines.push(String::new());
            lines.push(tty_style::header("action baseline"));
            lines.extend(render_action_table(&action_baseline));
        }
        lines.push(String::new());
        lines.push(format!(
            "{}  {}",
            tty_style::dim("manifest"),
            manifest_path.display()
        ));
    } else if let Some(hidden) = render_hidden_summary(baseline.len(), action_baseline.len()) {
        lines.push(String::new());
        lines.push(hidden);
    }

    lines
}

fn plural_suffix(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

fn count_label(count: usize, noun: &str) -> String {
    format!("{} {}{}", count, noun, plural_suffix(count))
}

fn render_report_summary_lines(
    manifest_path: &Path,
    summary: &AuditSummary,
    records: &[DepAuditRecord],
    actions: &[ActionAuditRecord],
    dep_baseline: usize,
    action_baseline: usize,
) -> Vec<String> {
    let manifest = manifest_path
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| manifest_path.to_string_lossy().into_owned());
    let dep_verdict = style_dependency_report_verdict(
        &format!("{:<5}", dependency_report_verdict(summary)),
        summary,
    );
    let mut lines = vec![format!(
        "{}  {:<7}  {}  {} {}  {}",
        dep_verdict,
        tty_style::header("deps"),
        tty_style::dim(manifest),
        tty_style::header(records.len().to_string()),
        tty_style::dim("deps"),
        render_dependency_summary_counts(summary, records, dep_baseline)
    )];

    if !actions.is_empty() {
        let action_verdict =
            style_action_report_verdict(&format!("{:<5}", action_report_verdict(actions)), actions);
        lines.push(format!(
            "{}  {:<7}  {}  {} {}  {}",
            action_verdict,
            tty_style::header("actions"),
            tty_style::dim("workflows"),
            tty_style::header(actions.len().to_string()),
            tty_style::dim("actions"),
            render_action_summary_counts(actions, action_baseline)
        ));
    }

    lines
}

fn dependency_report_verdict(summary: &AuditSummary) -> &'static str {
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

fn action_report_verdict(actions: &[ActionAuditRecord]) -> &'static str {
    if action_attention_counts(actions).1 > 0 {
        "WARN"
    } else {
        "OK"
    }
}

fn render_hidden_summary(dep_baseline: usize, action_baseline: usize) -> Option<String> {
    let mut parts = Vec::new();
    if dep_baseline > 0 {
        parts.push(format!(
            "{} baseline dep{}",
            dep_baseline,
            plural_suffix(dep_baseline)
        ));
    }
    if action_baseline > 0 {
        parts.push(format!(
            "{} baseline action{}",
            action_baseline,
            plural_suffix(action_baseline)
        ));
    }
    (!parts.is_empty()).then(|| {
        format!(
            "{}  {}; use `--verbose` to show all",
            tty_style::dim("hidden"),
            parts.join(", ")
        )
    })
}

fn render_dependency_summary_counts(
    summary: &AuditSummary,
    records: &[DepAuditRecord],
    baseline_count: usize,
) -> String {
    let mut parts = Vec::new();
    let (bump_count, review_count) = version_attention_counts(records);
    if bump_count > 0 {
        parts.push(tty_style::active(count_label(bump_count, "update")));
    }
    if review_count > 0 {
        parts.push(tty_style::warning(count_label(review_count, "review")));
    }
    if summary.high > 0 {
        parts.push(tty_style::error(format!("{} high", summary.high)));
    }
    if summary.medium > 0 {
        parts.push(tty_style::warning(format!("{} medium", summary.medium)));
    }
    if summary.unknown > 0 {
        parts.push(tty_style::active(format!("{} unknown", summary.unknown)));
    }
    if baseline_count > 0 {
        parts.push(tty_style::dim(format!("{baseline_count} baseline")));
    }
    if summary.skipped_local > 0 {
        parts.push(tty_style::dim(format!(
            "{} internal skipped",
            summary.skipped_local
        )));
    }
    if parts.is_empty() {
        tty_style::dim("no findings")
    } else {
        text_render::join_dim_bullets(&parts)
    }
}

fn render_action_summary_counts(actions: &[ActionAuditRecord], baseline_count: usize) -> String {
    let mut parts = Vec::new();
    let (action_bump_count, action_review_count) = action_attention_counts(actions);
    if action_bump_count > 0 {
        parts.push(tty_style::active(count_label(action_bump_count, "update")));
    }
    if action_review_count > 0 {
        parts.push(tty_style::warning(count_label(
            action_review_count,
            "review",
        )));
    }
    if baseline_count > 0 {
        parts.push(tty_style::dim(format!("{baseline_count} baseline")));
    }
    if parts.is_empty() {
        tty_style::dim("no findings")
    } else {
        text_render::join_dim_bullets(&parts)
    }
}

fn render_update_table(
    manifest_path: &Path,
    records: &[&DepAuditRecord],
    actions: &[&ActionAuditRecord],
) -> Vec<String> {
    let manifest = manifest_path
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| manifest_path.to_string_lossy().into_owned());
    let rows = records.len() + actions.len();
    let name_width = records
        .iter()
        .map(|record| record.name.chars().count())
        .chain(actions.iter().map(|record| record.action.chars().count()))
        .max()
        .unwrap_or(4)
        .max("name".chars().count())
        .min(32);
    let current_width = records
        .iter()
        .map(|record| record.requirement.chars().count())
        .chain(
            actions
                .iter()
                .map(|record| record.current_ref.chars().count()),
        )
        .max()
        .unwrap_or(7)
        .max("current".chars().count())
        .min(16);
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
        .chain(
            actions
                .iter()
                .map(|record| action_latest_label(record).chars().count()),
        )
        .max()
        .unwrap_or(6)
        .max("latest".chars().count())
        .min(16);
    let source_width = records
        .iter()
        .map(|_| manifest.chars().count())
        .chain(
            actions
                .iter()
                .map(|record| action_location_label(record).chars().count()),
        )
        .max()
        .unwrap_or(6)
        .max("source".chars().count())
        .min(32);

    let mut lines = Vec::with_capacity(rows + 1);
    lines.push(tty_style::dim(format!(
        "{:<6}  {:<name_width$}  {:<current_width$}  {:<latest_width$}  {:<source_width$}  note",
        "kind", "name", "current", "latest", "source"
    )));

    for record in records {
        let kind = tty_style::dim(format!("{:<6}", "crate"));
        let name = tty_style::header(format!(
            "{:<name_width$}",
            text_render::truncate_end(&record.name, name_width)
        ));
        let current = tty_style::dim(format!(
            "{:<current_width$}",
            text_render::truncate_end(&record.requirement, current_width)
        ));
        let latest = style_dep_latest_cell(
            &text_render::truncate_end(
                record.latest_version.as_deref().unwrap_or("-"),
                latest_width,
            ),
            latest_width,
            record.latest_version.is_some(),
        );
        let source = tty_style::dim(format!(
            "{:<source_width$}",
            text_render::truncate_end(&manifest, source_width)
        ));
        lines.push(format!(
            "{}  {}  {}  {}  {}  {}",
            kind,
            name,
            current,
            latest,
            source,
            summarize_record_note(record)
        ));
    }

    for record in actions {
        let kind = tty_style::dim(format!("{:<6}", "action"));
        let name = tty_style::header(format!(
            "{:<name_width$}",
            text_render::truncate_end(&record.action, name_width)
        ));
        let current = tty_style::dim(format!(
            "{:<current_width$}",
            text_render::truncate_end(&record.current_ref, current_width)
        ));
        let latest_value = action_latest_label(record);
        let latest = style_action_latest_cell(
            &text_render::truncate_end(&latest_value, latest_width),
            latest_width,
            latest_value != "-",
        );
        let source_label = action_location_label(record);
        let source = tty_style::dim(format!(
            "{:<source_width$}",
            text_render::truncate_end(&source_label, source_width)
        ));
        lines.push(format!(
            "{}  {}  {}  {}  {}  {}",
            kind,
            name,
            current,
            latest,
            source,
            compact_action_note(record.note.as_deref())
        ));
    }

    lines
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

fn render_action_table(records: &[&ActionAuditRecord]) -> Vec<String> {
    let action_width = records
        .iter()
        .map(|record| record.action.chars().count())
        .max()
        .unwrap_or(6)
        .clamp(6, 32);
    let ref_width = records
        .iter()
        .map(|record| record.current_ref.chars().count())
        .max()
        .unwrap_or(3)
        .clamp(3, 16);
    let latest_width = records
        .iter()
        .map(|record| action_latest_label(record).chars().count())
        .max()
        .unwrap_or(6)
        .clamp(6, 16);
    let file_width = records
        .iter()
        .map(|record| action_location_label(record).chars().count())
        .max()
        .unwrap_or(4)
        .clamp(4, 32);

    let mut lines = Vec::with_capacity(records.len() + 1);
    lines.push(tty_style::dim(format!(
        "{:<6}  {:<action_width$}  {:<ref_width$}  {:<latest_width$}  {:<file_width$}  note",
        "plan", "action", "ref", "latest", "file"
    )));
    for record in records {
        let plan = style_action_plan_cell(record.update_plan, 6);
        let action = tty_style::header(format!(
            "{:<action_width$}",
            text_render::truncate_end(&record.action, action_width)
        ));
        let current_ref = tty_style::dim(format!(
            "{:<ref_width$}",
            text_render::truncate_end(&record.current_ref, ref_width)
        ));
        let latest_value = action_latest_label(record);
        let latest = style_action_latest_cell(
            &text_render::truncate_end(&latest_value, latest_width),
            latest_width,
            latest_value != "-",
        );
        let file = tty_style::dim(format!(
            "{:<file_width$}",
            text_render::truncate_end(&action_location_label(record), file_width)
        ));
        lines.push(format!(
            "{}  {}  {}  {}  {}  {}",
            plan,
            action,
            current_ref,
            latest,
            file,
            compact_action_note(record.note.as_deref())
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

fn record_needs_attention(record: &DepAuditRecord) -> bool {
    record.risk != RiskLevel::Low
        || record
            .update_plan
            .is_some_and(DependencyUpdatePlan::needs_attention)
}

fn record_is_version_update(record: &DepAuditRecord) -> bool {
    matches!(
        record.update_plan,
        Some(DependencyUpdatePlan::Add | DependencyUpdatePlan::Bump)
    )
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

fn action_attention_counts(records: &[ActionAuditRecord]) -> (usize, usize) {
    let mut bump_count = 0;
    let mut review_count = 0;
    for record in records {
        match record.update_plan {
            ActionUpdatePlan::Bump => bump_count += 1,
            ActionUpdatePlan::Review => review_count += 1,
            ActionUpdatePlan::Keep => {}
        }
    }
    (bump_count, review_count)
}

fn action_needs_attention(record: &ActionAuditRecord) -> bool {
    record.update_plan.needs_attention()
}

fn action_location_label(record: &ActionAuditRecord) -> String {
    let Some(first) = record.locations.first() else {
        return "-".to_string();
    };
    if record.locations.len() == 1 {
        return format!("{}:{}", first.file, first.line);
    }
    format!(
        "{}:{} +{}",
        first.file,
        first.line,
        record.locations.len() - 1
    )
}

fn action_latest_label(record: &ActionAuditRecord) -> String {
    if record.update_plan == ActionUpdatePlan::Review
        && record.note.as_deref() == Some("floating or non-semver ref; review manually")
    {
        return "-".to_string();
    }
    record.latest_ref.as_deref().unwrap_or("-").to_string()
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

fn compact_action_note(note: Option<&str>) -> String {
    let Some(note) = note else {
        return tty_style::dim("-");
    };
    if note == "sha-pinned" {
        return "sha-pinned".to_string();
    }
    if note == "current ref is up to date" {
        return "latest".to_string();
    }
    if note == "newer action tag available" {
        return "newer-tag".to_string();
    }
    if note == "floating or non-semver ref; review manually" {
        return "floating-ref".to_string();
    }
    if note == "no semver tags found" {
        return "no-tags".to_string();
    }
    if note.starts_with("GitHub query failed:") {
        return "github-unavailable".to_string();
    }
    text_render::truncate_end(note, 24)
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
        return "github-unavailable".to_string();
    }
    if note.starts_with("crates.io query failed:") {
        return "crates.io?".to_string();
    }
    if note.starts_with("GitHub query failed:") {
        return "github-unavailable".to_string();
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

fn action_plan_label(plan: ActionUpdatePlan) -> &'static str {
    match plan {
        ActionUpdatePlan::Keep => "keep",
        ActionUpdatePlan::Bump => "bump",
        ActionUpdatePlan::Review => "review",
    }
}

fn style_action_plan_cell(plan: ActionUpdatePlan, width: usize) -> String {
    let padded = format!("{:<width$}", action_plan_label(plan));
    match plan {
        ActionUpdatePlan::Keep => tty_style::dim(padded),
        ActionUpdatePlan::Bump => tty_style::active(padded),
        ActionUpdatePlan::Review => tty_style::warning(padded),
    }
}

fn style_action_latest_cell(value: &str, width: usize, has_latest: bool) -> String {
    let padded = format!("{value:<width$}");
    if has_latest {
        tty_style::active(padded)
    } else {
        tty_style::dim(padded)
    }
}

fn style_dependency_report_verdict(value: &str, summary: &AuditSummary) -> String {
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

fn style_action_report_verdict(value: &str, actions: &[ActionAuditRecord]) -> String {
    if action_attention_counts(actions).1 > 0 {
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
    actions: &[ActionAuditRecord],
) -> Result<()> {
    let report = AuditReport {
        generated_at: format_rfc3339_seconds(SystemTime::now()).to_string(),
        manifest_path: manifest_path.display().to_string(),
        summary: summary.clone(),
        dependencies: records.to_vec(),
        actions: actions.to_vec(),
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
