use super::*;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CiState {
    Pending,
    Running,
    Success,
    Failed,
    Cancelled,
    Skipped,
    NoRuns,
}

impl CiState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
            Self::NoRuns => "no_runs",
        }
    }

    pub(crate) fn is_terminal(self) -> bool {
        !matches!(self, Self::Pending | Self::Running)
    }

    pub(crate) fn sort_weight(self) -> u8 {
        match self {
            Self::Failed => 0,
            Self::Cancelled => 1,
            Self::Running => 2,
            Self::Pending => 3,
            Self::NoRuns => 4,
            Self::Skipped => 5,
            Self::Success => 6,
        }
    }

    pub(crate) fn badge(self) -> &'static str {
        match self {
            Self::Pending => "PEND",
            Self::Running => "RUN",
            Self::Success => "OK",
            Self::Failed => "FAIL",
            Self::Cancelled => "CANC",
            Self::Skipped => "SKIP",
            Self::NoRuns => "NONE",
        }
    }
}

#[derive(Debug, Clone, Serialize, Default)]
pub(crate) struct CiSummary {
    pub(crate) pending: usize,
    pub(crate) running: usize,
    pub(crate) success: usize,
    pub(crate) failed: usize,
    pub(crate) cancelled: usize,
    pub(crate) skipped: usize,
}

impl CiSummary {
    pub(crate) fn push(&mut self, state: CiState) {
        match state {
            CiState::Pending => self.pending += 1,
            CiState::Running => self.running += 1,
            CiState::Success => self.success += 1,
            CiState::Failed => self.failed += 1,
            CiState::Cancelled => self.cancelled += 1,
            CiState::Skipped => self.skipped += 1,
            CiState::NoRuns => {}
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CiSourceKind {
    CurrentRepo,
    LocalPath,
    Repo,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WorkflowRunReport {
    pub(crate) id: u64,
    pub(crate) name: String,
    pub(crate) event: Option<String>,
    pub(crate) state: CiState,
    pub(crate) status: Option<String>,
    pub(crate) conclusion: Option<String>,
    pub(crate) run_attempt: Option<u64>,
    pub(crate) updated_at: Option<String>,
    pub(crate) html_url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WorkflowJobReport {
    pub(crate) id: u64,
    pub(crate) name: String,
    pub(crate) state: CiState,
    pub(crate) status: Option<String>,
    pub(crate) conclusion: Option<String>,
    pub(crate) html_url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) attention_steps: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WorkflowInspectReport {
    pub(crate) run: WorkflowRunReport,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) jobs: Vec<WorkflowJobReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) job_query_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CommitCiInspectReport {
    pub(crate) repo: String,
    pub(crate) sha: Option<String>,
    pub(crate) selected_all_runs: bool,
    pub(crate) state: CiState,
    pub(crate) summary: CiSummary,
    pub(crate) workflows: Vec<WorkflowInspectReport>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CommitCiReport {
    pub(crate) repo: String,
    pub(crate) branch: Option<String>,
    pub(crate) sha: Option<String>,
    pub(crate) state: CiState,
    pub(crate) summary: CiSummary,
    pub(crate) latest_update_at: Option<String>,
    pub(crate) source: CiSourceKind,
    pub(crate) source_path: Option<String>,
    pub(crate) runs: Vec<WorkflowRunReport>,
}

#[derive(Debug, Serialize, Default)]
pub(crate) struct CiBoardSummary {
    pub(crate) total: usize,
    pub(crate) errors: usize,
    pub(crate) pending: usize,
    pub(crate) running: usize,
    pub(crate) success: usize,
    pub(crate) failed: usize,
    pub(crate) cancelled: usize,
    pub(crate) skipped: usize,
    pub(crate) no_runs: usize,
}

impl CiBoardSummary {
    pub(crate) fn push_state(&mut self, state: CiState) {
        self.total += 1;
        match state {
            CiState::Pending => self.pending += 1,
            CiState::Running => self.running += 1,
            CiState::Success => self.success += 1,
            CiState::Failed => self.failed += 1,
            CiState::Cancelled => self.cancelled += 1,
            CiState::Skipped => self.skipped += 1,
            CiState::NoRuns => self.no_runs += 1,
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct CiBoardEntry {
    pub(crate) target: String,
    pub(crate) report: Option<CommitCiReport>,
    pub(crate) query_error: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct CiBoardOutput {
    pub(crate) summary: CiBoardSummary,
    pub(crate) entries: Vec<CiBoardEntry>,
}

#[derive(Debug, Clone)]
pub(crate) struct RepoSlug {
    pub(crate) owner: String,
    pub(crate) repo: String,
}

impl RepoSlug {
    pub(crate) fn as_str(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

#[derive(Debug)]
pub(crate) struct LocalRepoContext {
    pub(crate) repo_path: PathBuf,
    pub(crate) slug: RepoSlug,
    pub(crate) branch: Option<String>,
    pub(crate) sha: String,
}

#[derive(Debug)]
pub(crate) enum CiTarget {
    LocalPath(PathBuf),
    Remote(RepoSlug),
}

impl CiTarget {
    pub(crate) fn label(&self) -> String {
        match self {
            Self::LocalPath(path) => path.display().to_string(),
            Self::Remote(slug) => slug.as_str(),
        }
    }

    pub(crate) fn dedupe_key(&self) -> String {
        match self {
            Self::LocalPath(path) => format!("path:{}", path.display()),
            Self::Remote(slug) => format!("repo:{}", slug.as_str()),
        }
    }
}

#[derive(Default, Deserialize)]
pub(crate) struct CiManifest {
    #[serde(default)]
    pub(crate) groups: BTreeMap<String, CiManifestGroup>,
}

#[derive(Default, Deserialize)]
pub(crate) struct CiManifestGroup {
    #[serde(default)]
    pub(crate) repos: Vec<String>,
}

#[derive(Deserialize)]
pub(crate) struct WorkflowRunsResponse {
    #[serde(default)]
    pub(crate) workflow_runs: Vec<GitHubWorkflowRun>,
}

#[derive(Deserialize)]
pub(crate) struct WorkflowJobsResponse {
    #[serde(default)]
    pub(crate) jobs: Vec<GitHubWorkflowJob>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GitHubWorkflowRun {
    pub(crate) id: u64,
    pub(crate) name: Option<String>,
    pub(crate) display_title: Option<String>,
    pub(crate) event: Option<String>,
    pub(crate) head_branch: Option<String>,
    #[serde(default)]
    pub(crate) head_sha: String,
    pub(crate) status: Option<String>,
    pub(crate) conclusion: Option<String>,
    pub(crate) run_attempt: Option<u64>,
    pub(crate) updated_at: Option<String>,
    pub(crate) html_url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GitHubWorkflowJob {
    pub(crate) id: u64,
    pub(crate) name: Option<String>,
    pub(crate) status: Option<String>,
    pub(crate) conclusion: Option<String>,
    pub(crate) html_url: Option<String>,
    #[serde(default)]
    pub(crate) steps: Vec<GitHubWorkflowJobStep>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GitHubWorkflowJobStep {
    pub(crate) name: Option<String>,
    pub(crate) status: Option<String>,
    pub(crate) conclusion: Option<String>,
}

pub(crate) fn build_commit_report(
    slug: &RepoSlug,
    branch: Option<String>,
    sha: Option<String>,
    source: CiSourceKind,
    source_path: Option<String>,
    mut runs: Vec<GitHubWorkflowRun>,
) -> CommitCiReport {
    let mut reports = runs.drain(..).map(workflow_run_report).collect::<Vec<_>>();
    reports.sort_by(|a, b| {
        a.state
            .sort_weight()
            .cmp(&b.state.sort_weight())
            .then_with(|| a.name.cmp(&b.name))
    });

    let mut summary = CiSummary::default();
    for run in &reports {
        summary.push(run.state);
    }

    let latest_update_at = reports
        .iter()
        .filter_map(|run| run.updated_at.clone())
        .max();

    CommitCiReport {
        repo: slug.as_str(),
        branch,
        sha,
        state: aggregate_commit_state(&reports),
        summary,
        latest_update_at,
        source,
        source_path,
        runs: reports,
    }
}

pub(crate) fn build_inspect_report(
    client: &GitHubClient,
    slug: &RepoSlug,
    report: &CommitCiReport,
    all: bool,
) -> CommitCiInspectReport {
    let selected_runs = report
        .runs
        .iter()
        .filter(|run| all || matches!(run.state, CiState::Failed | CiState::Cancelled))
        .cloned()
        .collect::<Vec<_>>();
    let mut workflows = Vec::with_capacity(selected_runs.len());

    for run in selected_runs {
        match client.fetch_workflow_jobs(slug, run.id) {
            Ok(jobs) => workflows.push(WorkflowInspectReport {
                run,
                jobs: jobs
                    .into_iter()
                    .map(workflow_job_report)
                    .filter(|job| {
                        all || !job.state.is_terminal()
                            || matches!(job.state, CiState::Failed | CiState::Cancelled)
                    })
                    .collect(),
                job_query_error: None,
            }),
            Err(err) => workflows.push(WorkflowInspectReport {
                run,
                jobs: Vec::new(),
                job_query_error: Some(err.to_string()),
            }),
        }
    }

    workflows.sort_by(|a, b| {
        review_detail_priority(a.run.state)
            .cmp(&review_detail_priority(b.run.state))
            .then_with(|| a.run.name.cmp(&b.run.name))
    });

    CommitCiInspectReport {
        repo: report.repo.clone(),
        sha: report.sha.clone(),
        selected_all_runs: all,
        state: report.state,
        summary: report.summary.clone(),
        workflows,
    }
}

pub(crate) fn workflow_run_report(run: GitHubWorkflowRun) -> WorkflowRunReport {
    WorkflowRunReport {
        id: run.id,
        name: normalize_owned(run.name)
            .or_else(|| normalize_owned(run.display_title))
            .unwrap_or_else(|| format!("run-{}", run.id)),
        event: normalize_owned(run.event),
        state: workflow_run_state(run.status.as_deref(), run.conclusion.as_deref()),
        status: normalize_owned(run.status),
        conclusion: normalize_owned(run.conclusion),
        run_attempt: run.run_attempt,
        updated_at: normalize_owned(run.updated_at),
        html_url: normalize_owned(run.html_url),
    }
}

pub(crate) fn workflow_job_report(job: GitHubWorkflowJob) -> WorkflowJobReport {
    let state = workflow_run_state(job.status.as_deref(), job.conclusion.as_deref());
    WorkflowJobReport {
        id: job.id,
        name: normalize_owned(job.name).unwrap_or_else(|| format!("job-{}", job.id)),
        state,
        status: normalize_owned(job.status),
        conclusion: normalize_owned(job.conclusion),
        html_url: normalize_owned(job.html_url),
        attention_steps: job
            .steps
            .into_iter()
            .filter_map(|step| {
                let state = workflow_run_state(step.status.as_deref(), step.conclusion.as_deref());
                (!matches!(state, CiState::Success | CiState::Skipped))
                    .then(|| normalize_owned(step.name))
                    .flatten()
            })
            .collect(),
    }
}

pub(crate) fn workflow_run_state(status: Option<&str>, conclusion: Option<&str>) -> CiState {
    match status.map(|value| value.trim().to_ascii_lowercase()) {
        Some(status)
            if matches!(
                status.as_str(),
                "queued" | "requested" | "waiting" | "pending"
            ) =>
        {
            CiState::Pending
        }
        Some(status) if status == "in_progress" => CiState::Running,
        Some(status) if status == "completed" => match conclusion
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("success") => CiState::Success,
            Some("cancelled") => CiState::Cancelled,
            Some("neutral") | Some("skipped") => CiState::Skipped,
            Some("failure")
            | Some("startup_failure")
            | Some("timed_out")
            | Some("action_required")
            | Some("stale") => CiState::Failed,
            _ => CiState::Failed,
        },
        _ => {
            if conclusion.is_some() {
                workflow_run_state(Some("completed"), conclusion)
            } else {
                CiState::Running
            }
        }
    }
}

pub(crate) fn aggregate_commit_state(runs: &[WorkflowRunReport]) -> CiState {
    if runs.is_empty() {
        return CiState::NoRuns;
    }
    if runs.iter().any(|run| run.state == CiState::Running) {
        return CiState::Running;
    }
    if runs.iter().any(|run| run.state == CiState::Pending) {
        return CiState::Pending;
    }
    if runs.iter().any(|run| run.state == CiState::Failed) {
        return CiState::Failed;
    }
    if runs.iter().any(|run| run.state == CiState::Cancelled) {
        return CiState::Cancelled;
    }
    if runs.iter().any(|run| run.state == CiState::Success) {
        return CiState::Success;
    }
    CiState::Skipped
}

pub(crate) fn entry_sort_weight(entry: &CiBoardEntry) -> u8 {
    match &entry.query_error {
        Some(_) => 0,
        None => entry
            .report
            .as_ref()
            .map(|report| report.state.sort_weight() + 1)
            .unwrap_or(u8::MAX),
    }
}

pub(crate) fn exit_code_for_state(state: CiState) -> i32 {
    match state {
        CiState::Pending | CiState::Running => EXIT_RUNNING,
        CiState::Failed | CiState::Cancelled => EXIT_FAILED,
        CiState::NoRuns => EXIT_NO_RUNS,
        CiState::Success | CiState::Skipped => 0,
    }
}

pub(crate) fn exit_code_for_board(board: &CiBoardOutput) -> i32 {
    if board.summary.errors > 0 || board.summary.failed > 0 || board.summary.cancelled > 0 {
        return EXIT_FAILED;
    }
    if board.summary.running > 0 || board.summary.pending > 0 {
        return EXIT_RUNNING;
    }
    if board.summary.total > 0
        && board.summary.no_runs == board.summary.total
        && board.summary.errors == 0
    {
        return EXIT_NO_RUNS;
    }
    0
}

pub(crate) fn board_entries_for_text(
    board: &CiBoardOutput,
    show_all: bool,
) -> (Vec<&CiBoardEntry>, usize) {
    if show_all {
        return (board.entries.iter().collect(), 0);
    }

    let has_attention = board.entries.iter().any(entry_needs_attention);
    if !has_attention {
        return (board.entries.iter().collect(), 0);
    }

    let mut visible = Vec::with_capacity(board.entries.len());
    let mut hidden_success = 0usize;
    for entry in &board.entries {
        if is_clean_success_entry(entry) {
            hidden_success += 1;
            continue;
        }
        visible.push(entry);
    }
    (visible, hidden_success)
}

pub(crate) fn entry_needs_attention(entry: &CiBoardEntry) -> bool {
    entry.query_error.is_some()
        || entry
            .report
            .as_ref()
            .is_some_and(|report| report.state != CiState::Success)
}

pub(crate) fn is_clean_success_entry(entry: &CiBoardEntry) -> bool {
    entry.query_error.is_none()
        && entry
            .report
            .as_ref()
            .is_some_and(|report| report.state == CiState::Success)
}

pub(crate) fn watch_interval_for_state(state: CiState) -> Duration {
    match state {
        CiState::Pending | CiState::NoRuns => Duration::from_secs(WATCH_PENDING_INTERVAL_SECS),
        CiState::Running => Duration::from_secs(WATCH_RUNNING_INTERVAL_SECS),
        CiState::Success | CiState::Failed | CiState::Cancelled | CiState::Skipped => {
            Duration::from_secs(0)
        }
    }
}

pub(crate) fn report_digest(report: &CommitCiReport) -> String {
    let mut digest = format!(
        "{}:{}:{}:{}:{}:{}",
        report.state.as_str(),
        report.summary.pending,
        report.summary.running,
        report.summary.success,
        report.summary.failed,
        report.summary.cancelled + report.summary.skipped
    );
    for run in &report.runs {
        digest.push(':');
        digest.push_str(&format!(
            "{}:{}:{}",
            run.id,
            run.state.as_str(),
            run.run_attempt.unwrap_or_default()
        ));
    }
    digest
}

pub(crate) fn review_detail_priority(state: CiState) -> u8 {
    match state {
        CiState::Failed => 0,
        CiState::Cancelled => 1,
        CiState::Running => 2,
        CiState::Pending => 3,
        CiState::NoRuns => 4,
        CiState::Skipped => 5,
        CiState::Success => 6,
    }
}

pub(crate) fn latest_head_sha(runs: &[GitHubWorkflowRun]) -> Option<String> {
    runs.iter().find_map(|run| normalize_ref(&run.head_sha))
}

pub(crate) fn normalize_owned(value: Option<String>) -> Option<String> {
    let value = value?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

pub(crate) fn normalize_ref(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}
