use super::*;

pub(crate) fn print_watch_update(report: &CommitCiReport) {
    for line in render_watch_update_lines(report) {
        println!("{line}");
    }
}

pub(crate) fn print_commit_report(report: &CommitCiReport) {
    for line in render_commit_report_lines(report) {
        println!("{line}");
    }
}

pub(crate) fn print_board_output(board: &CiBoardOutput, show_all: bool) {
    for line in render_board_output_lines(board, show_all) {
        println!("{line}");
    }
}

pub(crate) fn format_summary_compact(summary: &CiSummary) -> String {
    let mut parts = Vec::new();
    if summary.failed > 0 {
        parts.push(ci_summary_token(summary.failed, "fail", CiState::Failed));
    }
    if summary.cancelled > 0 {
        parts.push(ci_summary_token(
            summary.cancelled,
            "cancel",
            CiState::Cancelled,
        ));
    }
    if summary.running > 0 {
        parts.push(ci_summary_token(summary.running, "run", CiState::Running));
    }
    if summary.pending > 0 {
        parts.push(ci_summary_token(summary.pending, "pend", CiState::Pending));
    }
    if summary.success > 0 {
        parts.push(ci_summary_token(summary.success, "ok", CiState::Success));
    }
    if summary.skipped > 0 {
        parts.push(ci_summary_token(summary.skipped, "skip", CiState::Skipped));
    }
    if parts.is_empty() {
        tty_style::dim("no runs")
    } else {
        text_render::join_dim_bullets(&parts)
    }
}

pub(crate) fn format_board_summary(summary: &CiBoardSummary) -> String {
    let mut parts = Vec::new();
    if summary.errors > 0 {
        parts.push(tty_style::error(format!("{} err", summary.errors)));
    }
    if summary.failed > 0 {
        parts.push(ci_summary_token(summary.failed, "fail", CiState::Failed));
    }
    if summary.cancelled > 0 {
        parts.push(ci_summary_token(
            summary.cancelled,
            "cancel",
            CiState::Cancelled,
        ));
    }
    if summary.running > 0 {
        parts.push(ci_summary_token(summary.running, "run", CiState::Running));
    }
    if summary.pending > 0 {
        parts.push(ci_summary_token(summary.pending, "pend", CiState::Pending));
    }
    if summary.success > 0 {
        parts.push(ci_summary_token(summary.success, "ok", CiState::Success));
    }
    if summary.no_runs > 0 {
        parts.push(ci_summary_token(summary.no_runs, "none", CiState::NoRuns));
    }
    if parts.is_empty() {
        tty_style::dim("no targets")
    } else {
        text_render::join_dim_bullets(&parts)
    }
}

pub(crate) fn render_commit_report_lines(report: &CommitCiReport) -> Vec<String> {
    let sha = report
        .sha
        .as_deref()
        .map(short_sha)
        .unwrap_or_else(|| "-".to_string());
    let updated = age_label(report.latest_update_at.as_deref()).unwrap_or_else(|| "-".to_string());
    let mut lines = vec![format!(
        "{} {}  {}  {} {}  {}",
        style_ci_badge(report.state, 5),
        tty_style::header(&report.repo),
        tty_style::dim(sha),
        tty_style::dim("updated"),
        tty_style::dim(updated),
        format_summary_compact(&report.summary)
    )];

    if report.runs.is_empty() {
        lines.push(tty_style::dim("no workflow runs found for this commit"));
        return lines;
    }

    lines.push(tty_style::header("actions"));
    for run in ordered_review_runs(&report.runs) {
        lines.push(render_run_detail_line(run, report));
    }
    if report
        .runs
        .iter()
        .any(|run| matches!(run.state, CiState::Failed | CiState::Cancelled))
    {
        lines.push(String::new());
        lines.push(tty_style::dim("inspect failures with `za gh ci inspect`"));
    }

    lines
}

pub(crate) fn render_board_output_lines(board: &CiBoardOutput, show_all: bool) -> Vec<String> {
    let mut lines = vec![format!(
        "{}  {} {}  {}",
        tty_style::header("CI"),
        tty_style::dim("total"),
        tty_style::header(board.summary.total.to_string()),
        format_board_summary(&board.summary)
    )];
    if board.entries.is_empty() {
        lines.push(tty_style::dim("No CI targets found."));
        return lines;
    }
    let (visible_entries, hidden_success) = board_entries_for_text(board, show_all);

    lines.push(tty_style::dim(format!(
        "{:<5} {:<28} {:<12} {:<7} {:<5} {:>3} {:>4} {:>2}  DETAIL",
        "ST", "REPO", "BRANCH", "SHA", "AGE", "RUN", "FAIL", "OK"
    )));

    for entry in visible_entries {
        match (&entry.report, &entry.query_error) {
            (_, Some(err)) => {
                lines.push(format!(
                    "{} {:<28} {:<12} {:<7} {:<5} {:>3} {:>4} {:>2}  {}",
                    style_error_badge(5),
                    text_render::truncate_end(&entry.target, 28),
                    style_ci_dim_field("-", 12),
                    style_ci_dim_field("-", 7),
                    style_ci_dim_field("-", 5),
                    style_ci_dim_number("-", 3),
                    style_ci_dim_number("-", 4),
                    style_ci_dim_number("-", 2),
                    text_render::truncate_end(err, 80)
                ));
            }
            (Some(report), None) => {
                let active = report.summary.running + report.summary.pending;
                let failures = report.summary.failed + report.summary.cancelled;
                let success = report.summary.success;
                let sha = report
                    .sha
                    .as_deref()
                    .map(short_sha)
                    .unwrap_or_else(|| "-".to_string());
                let age = age_label(report.latest_update_at.as_deref())
                    .unwrap_or_else(|| "-".to_string());
                lines.push(format!(
                    "{} {} {:<12} {:<7} {:<5} {:>3} {:>4} {:>2}  {}",
                    style_ci_badge(report.state, 5),
                    style_ci_repo_field(&report.repo, 28),
                    style_ci_dim_field(report.branch.as_deref().unwrap_or("-"), 12),
                    style_ci_dim_field(&sha, 7),
                    style_ci_dim_field(&age, 5),
                    style_ci_metric(active, 3, CiState::Running),
                    style_ci_metric(failures, 4, CiState::Failed),
                    style_ci_metric(success, 2, CiState::Success),
                    text_render::truncate_end(&board_detail(report), 80)
                ));
            }
            _ => {}
        }
    }

    if hidden_success > 0 {
        lines.push(String::new());
        lines.push(format!(
            "{} {} clean green target(s) hidden; pass `--all` to show them",
            tty_style::dim("..."),
            hidden_success
        ));
    }

    lines
}

pub(crate) fn render_watch_update_lines(report: &CommitCiReport) -> Vec<String> {
    let sha = report
        .sha
        .as_deref()
        .map(short_sha)
        .unwrap_or_else(|| "-".to_string());
    let updated = age_label(report.latest_update_at.as_deref()).unwrap_or_else(|| "-".to_string());
    let mut lines = vec![format!(
        "{} {}  {}  {} {}  {}",
        style_ci_badge(report.state, 5),
        tty_style::header(&report.repo),
        tty_style::dim(sha),
        tty_style::dim("updated"),
        tty_style::dim(updated),
        format_summary_compact(&report.summary)
    )];

    if !matches!(report.state, CiState::Pending | CiState::Running) {
        return lines;
    }

    let detail_runs = watch_detail_runs(report);
    let hidden_runs = detail_runs.len().saturating_sub(WATCH_DETAIL_LIMIT);
    for run in detail_runs.iter().take(WATCH_DETAIL_LIMIT) {
        lines.push(render_run_detail_line(run, report));
    }
    if hidden_runs > 0 {
        lines.push(format!(
            "  {} {} more active workflow{}",
            tty_style::dim("..."),
            hidden_runs,
            text_render::pluralize(hidden_runs, "", "s")
        ));
    }
    lines
}

pub(crate) fn render_inspect_report_lines(report: &CommitCiInspectReport) -> Vec<String> {
    let sha = report
        .sha
        .as_deref()
        .map(short_sha)
        .unwrap_or_else(|| "-".to_string());
    let selected = report.workflows.len();
    let mut lines = vec![format!(
        "{} {}  {}  {} {}",
        style_ci_badge(report.state, 5),
        tty_style::header(&report.repo),
        tty_style::dim(sha),
        tty_style::header(selected.to_string()),
        tty_style::dim(if selected == 1 {
            "workflow inspected"
        } else {
            "workflows inspected"
        })
    )];

    if report.workflows.is_empty() {
        lines.push(tty_style::dim(if report.selected_all_runs {
            "No workflows matched for this commit."
        } else {
            "No failed or cancelled workflows for this commit."
        }));
        return lines;
    }

    lines.push(tty_style::header("inspect"));
    for workflow in &report.workflows {
        lines.push(format!(
            "  {} {}",
            style_ci_badge(workflow.run.state, 5),
            style_ci_subject(
                &text_render::truncate_end(&workflow.run.name, 92),
                workflow.run.state
            )
        ));
        if let Some(url) = &workflow.run.html_url {
            lines.push(format!(
                "    {} {}",
                tty_style::dim("url"),
                tty_style::dim(url)
            ));
        }
        if let Some(err) = &workflow.job_query_error {
            lines.push(format!(
                "    {} {}",
                tty_style::warning("jobs"),
                text_render::truncate_end(err, 120)
            ));
            continue;
        }
        if workflow.jobs.is_empty() {
            lines.push(format!(
                "    {} {}",
                tty_style::dim("jobs"),
                tty_style::dim("no matching jobs")
            ));
            continue;
        }
        for job in &workflow.jobs {
            lines.push(format!(
                "    {} {}",
                style_ci_badge(job.state, 5),
                style_ci_subject(&text_render::truncate_end(&job.name, 88), job.state)
            ));
            if let Some(url) = &job.html_url {
                lines.push(format!(
                    "      {} {}",
                    tty_style::dim("url"),
                    tty_style::dim(url)
                ));
            }
            for step in &job.attention_steps {
                lines.push(format!(
                    "      {} {}",
                    tty_style::dim("step"),
                    text_render::truncate_end(step, 96)
                ));
            }
        }
    }

    lines
}

pub(crate) fn ordered_review_runs(runs: &[WorkflowRunReport]) -> Vec<&WorkflowRunReport> {
    let mut runs = runs.iter().collect::<Vec<_>>();
    runs.sort_by(|a, b| {
        review_detail_priority(a.state)
            .cmp(&review_detail_priority(b.state))
            .then_with(|| a.name.cmp(&b.name))
    });
    runs
}

pub(crate) fn watch_detail_runs(report: &CommitCiReport) -> Vec<&WorkflowRunReport> {
    let mut runs = report
        .runs
        .iter()
        .filter(|run| !matches!(run.state, CiState::Success | CiState::Skipped))
        .collect::<Vec<_>>();
    runs.sort_by(|a, b| {
        review_detail_priority(a.state)
            .cmp(&review_detail_priority(b.state))
            .then_with(|| a.name.cmp(&b.name))
    });
    runs
}

pub(crate) fn render_run_detail_line(run: &WorkflowRunReport, report: &CommitCiReport) -> String {
    let age = age_label(run.updated_at.as_deref()).unwrap_or_else(|| "-".to_string());
    let mut detail = if has_mixed_events(&report.runs) {
        format!(
            "{} {}",
            style_ci_dim_field(
                &text_render::truncate_end(run.event.as_deref().unwrap_or("-"), 8),
                8
            ),
            style_ci_subject(&text_render::truncate_end(&run.name, 80), run.state)
        )
    } else {
        style_ci_subject(&text_render::truncate_end(&run.name, 88), run.state)
    };
    if let Some(attempt) = run.run_attempt
        && attempt > 1
    {
        detail.push_str(&format!("  {}", tty_style::dim(format!("#{attempt}"))));
    }
    format!(
        "  {} {} {}",
        style_ci_badge(run.state, 5),
        style_ci_dim_field(&age, 5),
        detail
    )
}

pub(crate) fn board_detail(report: &CommitCiReport) -> String {
    let mut parts = Vec::new();
    if report.summary.cancelled > 0 {
        parts.push(tty_style::dim(format!(
            "{} cancel",
            report.summary.cancelled
        )));
    }
    if report.summary.skipped > 0 {
        parts.push(tty_style::dim(format!("{} skip", report.summary.skipped)));
    }
    if report.state == CiState::NoRuns {
        parts.push(tty_style::dim("no runs"));
    }
    if parts.is_empty() {
        tty_style::dim("-")
    } else {
        text_render::join_dim_bullets(&parts)
    }
}

pub(crate) fn ci_summary_token(count: usize, label: &str, state: CiState) -> String {
    let text = if count == 0 && label == "no runs" {
        label.to_string()
    } else {
        format!("{count} {label}")
    };
    match state {
        CiState::Success => tty_style::success(text),
        CiState::Failed => tty_style::error(text),
        CiState::Cancelled | CiState::Pending => tty_style::warning(text),
        CiState::Running => tty_style::active(text),
        CiState::Skipped | CiState::NoRuns => tty_style::dim(text),
    }
}

pub(crate) fn style_ci_badge(state: CiState, width: usize) -> String {
    let label = format!("{:<width$}", state.badge());
    match state {
        CiState::Success => tty_style::success(label),
        CiState::Failed => tty_style::error(label),
        CiState::Cancelled | CiState::Pending => tty_style::warning(label),
        CiState::Running => tty_style::active(label),
        CiState::Skipped | CiState::NoRuns => tty_style::dim(label),
    }
}

pub(crate) fn style_error_badge(width: usize) -> String {
    tty_style::error(format!("{:<width$}", "ERR"))
}

pub(crate) fn style_ci_repo_field(value: &str, width: usize) -> String {
    tty_style::header(format!(
        "{:<width$}",
        text_render::truncate_end(value, width)
    ))
}

pub(crate) fn style_ci_dim_field(value: &str, width: usize) -> String {
    tty_style::dim(format!(
        "{:<width$}",
        text_render::truncate_end(value, width)
    ))
}

pub(crate) fn style_ci_dim_number(value: &str, width: usize) -> String {
    tty_style::dim(format!("{value:>width$}"))
}

pub(crate) fn style_ci_metric(value: usize, width: usize, state: CiState) -> String {
    let plain = format!("{value:>width$}");
    if value == 0 {
        return tty_style::dim(plain);
    }
    match state {
        CiState::Success => tty_style::success(plain),
        CiState::Failed => tty_style::error(plain),
        CiState::Cancelled | CiState::Pending => tty_style::warning(plain),
        CiState::Running => tty_style::active(plain),
        CiState::Skipped | CiState::NoRuns => tty_style::dim(plain),
    }
}

pub(crate) fn style_ci_subject(value: &str, state: CiState) -> String {
    match state {
        CiState::Success => tty_style::success(value),
        CiState::Failed => tty_style::error(value),
        CiState::Cancelled | CiState::Pending => tty_style::warning(value),
        CiState::Running => tty_style::active(value),
        CiState::Skipped | CiState::NoRuns => tty_style::header(value),
    }
}

pub(crate) fn has_mixed_events(runs: &[WorkflowRunReport]) -> bool {
    let mut distinct = BTreeSet::new();
    for run in runs {
        distinct.insert(run.event.as_deref().unwrap_or("-"));
        if distinct.len() > 1 {
            return true;
        }
    }
    false
}

pub(crate) fn age_label(rfc3339: Option<&str>) -> Option<String> {
    let value = rfc3339?;
    let ts = parse_rfc3339_weak(value).ok()?;
    let elapsed = match SystemTime::now().duration_since(ts) {
        Ok(duration) => duration,
        Err(_) => Duration::from_secs(0),
    };
    Some(format_duration_short(elapsed))
}

pub(crate) fn format_duration_short(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs < 60 {
        return format!("{secs}s");
    }
    if secs < 3_600 {
        return format!("{}m", secs / 60);
    }
    if secs < 86_400 {
        return format!("{}h", secs / 3_600);
    }
    format!("{}d", secs / 86_400)
}

pub(crate) fn short_sha(value: &str) -> String {
    value.chars().take(7).collect()
}
