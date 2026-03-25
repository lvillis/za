use super::*;
use anyhow::{Context, Result, bail};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar},
    widgets::{ScrollbarOrientation, ScrollbarState, Wrap},
};
use std::{
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{Duration, Instant, SystemTime},
};

const DIFF_TUI_REFRESH_INTERVAL: Duration = Duration::from_millis(900);
const DIFF_TUI_EVENT_POLL_INTERVAL: Duration = Duration::from_millis(120);
const DIFF_TUI_SELECTION_HEIGHT: u16 = 7;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiffTuiFocus {
    Files,
    Patch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiffTuiGrouping {
    Flat,
    Category,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiffTuiLayoutMode {
    Auto,
    Split,
    Stacked,
    PatchOnly,
    FilesOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiffTuiResolvedLayout {
    Split,
    Stacked,
    PatchOnly,
    FilesOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiffTuiRowDensity {
    Minimal,
    Compact,
    Wide,
}

#[derive(Clone, Copy, Debug)]
struct DiffTuiScopeFilter {
    unstaged: bool,
    staged: bool,
    untracked: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DiffSelectionKey {
    path: String,
    previous_path: Option<String>,
}

#[derive(Clone, Debug)]
struct DiffPatchLine {
    text: String,
    kind: DiffPatchLineKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiffPatchLineKind {
    Section,
    MetaInfo,
    MetaBoilerplate,
    Hunk,
    Addition,
    Deletion,
    Plain,
    Dim,
    Error,
}

#[derive(Clone, Debug, Default)]
struct DiffPatchPreview {
    lines: Vec<DiffPatchLine>,
    loaded_at: Option<SystemTime>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DiffPatchCacheKey {
    selection: DiffSelectionKey,
    status: DiffStatus,
    scopes: Vec<DiffScope>,
    additions: u64,
    deletions: u64,
    binary: bool,
}

#[derive(Clone, Copy, Debug)]
enum DiffListRow<'a> {
    Header(&'a str),
    Entry(usize),
}

#[derive(Clone, Debug, Default)]
struct DiffPatchRenderView<'a> {
    visible_lines: Vec<&'a DiffPatchLine>,
    raw_indices: Vec<usize>,
    hunk_rows: Vec<usize>,
}

#[derive(Debug)]
struct DiffTuiApp {
    repo_root: PathBuf,
    repo_name: String,
    base_filters: DiffFilterSpec,
    scope_filter: DiffTuiScopeFilter,
    report: Option<DiffWorkspaceOutput>,
    display_entries: Vec<DiffFileStat>,
    selected: usize,
    list_scroll_offset: usize,
    list_viewport_rows: usize,
    patch_scroll_y: usize,
    patch_scroll_x: usize,
    patch_viewport_rows: usize,
    patch_viewport_cols: usize,
    focus: DiffTuiFocus,
    show_help: bool,
    grouping: DiffTuiGrouping,
    layout_mode: DiffTuiLayoutMode,
    show_patch_boilerplate: bool,
    workspace_signature: Option<Vec<u8>>,
    patch_cache_key: Option<DiffPatchCacheKey>,
    last_scan_at: Option<SystemTime>,
    last_refresh_at: Option<SystemTime>,
    last_refresh_tick: Option<Instant>,
    last_refresh_error: Option<String>,
    status_message: Option<String>,
    patch_preview: DiffPatchPreview,
}

#[derive(Clone, Debug)]
struct DiffPatchSpec {
    scope: DiffScope,
    args: Vec<String>,
    allowed_codes: &'static [i32],
}

pub(super) fn run_tui(options: DiffRunOptions) -> Result<i32> {
    if !io::stdout().is_terminal() {
        bail!("`za diff --tui` requires a TTY");
    }

    let repo_root = resolve_repo_root()?;
    let base_filters = DiffFilterSpec::from_run_options(&options, &repo_root)?;
    let mut app = DiffTuiApp::new(repo_root, base_filters);
    app.refresh(true)?;

    enable_raw_mode().context("enable raw terminal mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create ratatui terminal")?;

    let result = run_tui_loop(&mut terminal, &mut app);

    let mut teardown_err: Option<anyhow::Error> = None;
    if let Err(err) = disable_raw_mode().context("disable raw terminal mode") {
        teardown_err = Some(err);
    }
    if let Err(err) =
        execute!(terminal.backend_mut(), LeaveAlternateScreen).context("leave alternate screen")
    {
        teardown_err = Some(match teardown_err {
            Some(prev) => prev.context(format!("{err:#}")),
            None => err,
        });
    }
    if let Err(err) = terminal.show_cursor().context("restore cursor visibility") {
        teardown_err = Some(match teardown_err {
            Some(prev) => prev.context(format!("{err:#}")),
            None => err,
        });
    }

    result?;
    if let Some(err) = teardown_err {
        return Err(err);
    }
    Ok(0)
}

fn run_tui_loop(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    app: &mut DiffTuiApp,
) -> Result<()> {
    loop {
        app.refresh(false)?;
        terminal
            .draw(|frame| draw_diff_tui(frame, app))
            .context("draw diff tui")?;

        if !event::poll(DIFF_TUI_EVENT_POLL_INTERVAL).context("poll diff tui events")? {
            continue;
        }

        match event::read().context("read diff tui event")? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if app.handle_key(key.code)? {
                    return Ok(());
                }
            }
            Event::Resize(_, _) => {}
            _ => {}
        }
    }
}

impl DiffTuiApp {
    fn new(repo_root: PathBuf, base_filters: DiffFilterSpec) -> Self {
        let repo_name = repo_root
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| repo_root.to_str().unwrap_or("repo"))
            .to_string();
        Self {
            repo_root,
            repo_name,
            scope_filter: DiffTuiScopeFilter::from_initial(&base_filters.summary.scopes),
            base_filters,
            report: None,
            display_entries: Vec::new(),
            selected: 0,
            list_scroll_offset: 0,
            list_viewport_rows: 0,
            patch_scroll_y: 0,
            patch_scroll_x: 0,
            patch_viewport_rows: 0,
            patch_viewport_cols: 0,
            focus: DiffTuiFocus::Files,
            show_help: false,
            grouping: DiffTuiGrouping::Category,
            layout_mode: DiffTuiLayoutMode::Auto,
            show_patch_boilerplate: false,
            workspace_signature: None,
            patch_cache_key: None,
            last_scan_at: None,
            last_refresh_at: None,
            last_refresh_tick: None,
            last_refresh_error: None,
            status_message: None,
            patch_preview: DiffPatchPreview::default(),
        }
    }

    fn refresh(&mut self, force: bool) -> Result<()> {
        if !force
            && self
                .last_refresh_tick
                .is_some_and(|tick| tick.elapsed() < DIFF_TUI_REFRESH_INTERVAL)
        {
            return Ok(());
        }

        let signature = workspace_signature(&self.repo_root)?;
        self.last_scan_at = Some(SystemTime::now());
        let previous_key = self.selected_key();
        if !force
            && self
                .workspace_signature
                .as_ref()
                .is_some_and(|current| current == &signature)
        {
            self.last_refresh_tick = Some(Instant::now());
            return Ok(());
        }

        let filters = self.effective_filters();
        match collect_workspace_diff(&self.repo_root, true, &filters) {
            Ok(report) => {
                self.workspace_signature = Some(signature);
                self.report = Some(report);
                self.rebuild_display_entries();
                self.last_refresh_at = Some(SystemTime::now());
                self.last_refresh_tick = Some(Instant::now());
                self.last_refresh_error = None;
                let selection_changed = self.restore_selection(previous_key);
                self.reload_patch(force || selection_changed);
            }
            Err(err) => {
                self.last_refresh_tick = Some(Instant::now());
                let message = format!("refresh failed: {err:#}");
                self.last_refresh_error = Some(message.clone());
                self.status_message = Some(message);
            }
        }
        Ok(())
    }

    fn handle_key(&mut self, code: KeyCode) -> Result<bool> {
        if self.show_help {
            match code {
                KeyCode::Char('?') | KeyCode::Esc => self.show_help = false,
                KeyCode::Char('q') => return Ok(true),
                _ => {}
            }
            return Ok(false);
        }

        match code {
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Tab | KeyCode::Enter => self.toggle_focus(),
            KeyCode::Esc => self.focus = DiffTuiFocus::Files,
            KeyCode::Char('v') => self.cycle_layout_mode(),
            KeyCode::Char('c') => self.toggle_grouping(),
            KeyCode::Char('m') => self.toggle_patch_boilerplate(),
            KeyCode::Char('r') => {
                self.status_message = Some("manual refresh".to_string());
                self.refresh(true)?;
            }
            KeyCode::Char('a') => {
                self.scope_filter = DiffTuiScopeFilter::all();
                self.status_message = Some("scope filter reset to all".to_string());
                self.refresh(true)?;
            }
            KeyCode::Char('u') => self.toggle_scope(DiffScope::Unstaged)?,
            KeyCode::Char('s') => self.toggle_scope(DiffScope::Staged)?,
            KeyCode::Char('n') => self.toggle_scope(DiffScope::Untracked)?,
            KeyCode::Char('[') => self.jump_risk(true),
            KeyCode::Char(']') => self.jump_risk(false),
            KeyCode::Char('{') => self.jump_hunk(true),
            KeyCode::Char('}') => self.jump_hunk(false),
            KeyCode::Char('g') | KeyCode::Home => self.scroll_home(),
            KeyCode::Char('G') | KeyCode::End => self.scroll_end(),
            KeyCode::PageUp => self.page_up(),
            KeyCode::PageDown => self.page_down(),
            KeyCode::Up | KeyCode::Char('k') => self.scroll_up(),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_down(),
            KeyCode::Left | KeyCode::Char('h') => self.scroll_patch_left(),
            KeyCode::Right | KeyCode::Char('l') => self.scroll_patch_right(),
            KeyCode::Char('0') => {
                if self.focus == DiffTuiFocus::Patch {
                    self.patch_scroll_x = 0;
                }
            }
            _ => {}
        }
        Ok(false)
    }

    fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            DiffTuiFocus::Files => DiffTuiFocus::Patch,
            DiffTuiFocus::Patch => DiffTuiFocus::Files,
        };
    }

    fn cycle_layout_mode(&mut self) {
        self.layout_mode = match self.layout_mode {
            DiffTuiLayoutMode::Auto => DiffTuiLayoutMode::Split,
            DiffTuiLayoutMode::Split => DiffTuiLayoutMode::Stacked,
            DiffTuiLayoutMode::Stacked => DiffTuiLayoutMode::PatchOnly,
            DiffTuiLayoutMode::PatchOnly => DiffTuiLayoutMode::FilesOnly,
            DiffTuiLayoutMode::FilesOnly => DiffTuiLayoutMode::Auto,
        };
        self.status_message = Some(format!("layout: {}", self.layout_mode.label()));
    }

    fn toggle_grouping(&mut self) {
        let previous_key = self.selected_key();
        self.grouping = match self.grouping {
            DiffTuiGrouping::Flat => DiffTuiGrouping::Category,
            DiffTuiGrouping::Category => DiffTuiGrouping::Flat,
        };
        self.rebuild_display_entries();
        self.restore_selection(previous_key);
        self.status_message = Some(format!("grouping: {}", self.grouping.label()));
    }

    fn toggle_patch_boilerplate(&mut self) {
        self.show_patch_boilerplate = !self.show_patch_boilerplate;
        self.patch_scroll_y = 0;
        self.status_message = Some(format!(
            "patch metadata: {}",
            if self.show_patch_boilerplate {
                "full"
            } else {
                "compact"
            }
        ));
    }

    fn toggle_scope(&mut self, scope: DiffScope) -> Result<()> {
        if !self.scope_filter.toggle(scope) {
            self.status_message = Some("at least one scope must remain visible".to_string());
            return Ok(());
        }
        self.status_message = Some(format!(
            "scope filter: {}",
            self.scope_filter.summary_label()
        ));
        self.refresh(true)
    }

    fn jump_risk(&mut self, reverse: bool) {
        let target = {
            let entries = self.visible_entries();
            if entries.is_empty() {
                None
            } else {
                let len = entries.len();
                let mut found = None;
                for offset in 1..=len {
                    let index = if reverse {
                        (self.selected + len - offset) % len
                    } else {
                        (self.selected + offset) % len
                    };
                    if !entries[index].risks.is_empty() {
                        found = Some(index);
                        break;
                    }
                }
                found
            }
        };
        if let Some(index) = target {
            self.set_selected(index);
        }
    }

    fn jump_hunk(&mut self, reverse: bool) {
        let hunk_rows = self.patch_render_view().hunk_rows;
        if hunk_rows.is_empty() {
            self.status_message = Some("no patch hunk markers in current preview".to_string());
            return;
        }
        self.focus = DiffTuiFocus::Patch;
        let current = self.patch_scroll_y;
        let target = if reverse {
            hunk_rows
                .iter()
                .rev()
                .copied()
                .find(|row| *row < current)
                .or_else(|| hunk_rows.last().copied())
        } else {
            hunk_rows
                .iter()
                .copied()
                .find(|row| *row > current)
                .or_else(|| hunk_rows.first().copied())
        };
        if let Some(target) = target {
            self.patch_scroll_y = target;
        }
    }

    fn scroll_home(&mut self) {
        match self.focus {
            DiffTuiFocus::Files => self.set_selected(0),
            DiffTuiFocus::Patch => self.patch_scroll_y = 0,
        }
    }

    fn scroll_end(&mut self) {
        match self.focus {
            DiffTuiFocus::Files => {
                let last = self.visible_entries().len().saturating_sub(1);
                self.set_selected(last);
            }
            DiffTuiFocus::Patch => {
                let lines = self.patch_preview.lines.len();
                let viewport = self.patch_viewport_rows.max(1);
                self.patch_scroll_y = lines.saturating_sub(viewport);
            }
        }
    }

    fn page_up(&mut self) {
        match self.focus {
            DiffTuiFocus::Files => {
                let delta = self.list_viewport_rows.max(1);
                self.set_selected(self.selected.saturating_sub(delta));
            }
            DiffTuiFocus::Patch => {
                let delta = self.patch_viewport_rows.max(1);
                self.patch_scroll_y = self.patch_scroll_y.saturating_sub(delta);
            }
        }
    }

    fn page_down(&mut self) {
        match self.focus {
            DiffTuiFocus::Files => {
                let delta = self.list_viewport_rows.max(1);
                let last = self.visible_entries().len().saturating_sub(1);
                self.set_selected(self.selected.saturating_add(delta).min(last));
            }
            DiffTuiFocus::Patch => {
                let delta = self.patch_viewport_rows.max(1);
                let max_scroll = self
                    .patch_preview
                    .lines
                    .len()
                    .saturating_sub(self.patch_viewport_rows.max(1));
                self.patch_scroll_y = self.patch_scroll_y.saturating_add(delta).min(max_scroll);
            }
        }
    }

    fn scroll_up(&mut self) {
        match self.focus {
            DiffTuiFocus::Files => {
                if self.selected > 0 {
                    self.set_selected(self.selected - 1);
                }
            }
            DiffTuiFocus::Patch => {
                self.patch_scroll_y = self.patch_scroll_y.saturating_sub(1);
            }
        }
    }

    fn scroll_down(&mut self) {
        match self.focus {
            DiffTuiFocus::Files => {
                let last = self.visible_entries().len().saturating_sub(1);
                if self.selected < last {
                    self.set_selected(self.selected + 1);
                }
            }
            DiffTuiFocus::Patch => {
                let max_scroll = self
                    .patch_preview
                    .lines
                    .len()
                    .saturating_sub(self.patch_viewport_rows.max(1));
                self.patch_scroll_y = self.patch_scroll_y.saturating_add(1).min(max_scroll);
            }
        }
    }

    fn scroll_patch_left(&mut self) {
        if self.focus == DiffTuiFocus::Patch {
            self.patch_scroll_x = self.patch_scroll_x.saturating_sub(4);
        }
    }

    fn scroll_patch_right(&mut self) {
        if self.focus == DiffTuiFocus::Patch {
            let max_scroll = self
                .patch_preview
                .lines
                .iter()
                .map(|line| line.text.chars().count())
                .max()
                .unwrap_or_default()
                .saturating_sub(self.patch_viewport_cols.max(1));
            self.patch_scroll_x = self.patch_scroll_x.saturating_add(4).min(max_scroll);
        }
    }

    fn set_selected(&mut self, index: usize) {
        let last = self.visible_entries().len().saturating_sub(1);
        let next = index.min(last);
        if next != self.selected {
            self.selected = next;
            self.patch_scroll_y = 0;
            self.patch_scroll_x = 0;
            self.reload_patch(true);
        }
    }

    fn restore_selection(&mut self, previous_key: Option<DiffSelectionKey>) -> bool {
        let (is_empty, resolved) = {
            let entries = self.visible_entries();
            if entries.is_empty() {
                (true, 0usize)
            } else {
                let resolved = previous_key
                    .and_then(|key| entries.iter().position(|entry| selected_key(entry) == key))
                    .unwrap_or_else(|| self.selected.min(entries.len().saturating_sub(1)));
                (false, resolved)
            }
        };

        if is_empty {
            let changed = self.selected != 0;
            self.selected = 0;
            self.list_scroll_offset = 0;
            self.patch_scroll_y = 0;
            self.patch_scroll_x = 0;
            return changed;
        }

        let changed = resolved != self.selected;
        self.selected = resolved;
        if changed {
            self.patch_scroll_y = 0;
            self.patch_scroll_x = 0;
        }
        changed
    }

    fn reload_patch(&mut self, selection_changed: bool) {
        let entry = self.selected_entry().cloned();
        let patch_key = entry.as_ref().map(DiffPatchCacheKey::from);
        if !selection_changed
            && patch_key.is_some()
            && self.patch_cache_key.as_ref() == patch_key.as_ref()
        {
            return;
        }
        self.patch_preview = match entry {
            Some(entry) => match load_patch_preview(&self.repo_root, &entry) {
                Ok(preview) => preview,
                Err(err) => DiffPatchPreview {
                    lines: vec![DiffPatchLine {
                        text: format!("patch preview unavailable: {err:#}"),
                        kind: DiffPatchLineKind::Error,
                    }],
                    loaded_at: Some(SystemTime::now()),
                },
            },
            None => DiffPatchPreview {
                lines: vec![DiffPatchLine {
                    text: "No file is currently selected.".to_string(),
                    kind: DiffPatchLineKind::Dim,
                }],
                loaded_at: Some(SystemTime::now()),
            },
        };
        self.patch_cache_key = patch_key;
        if selection_changed {
            self.patch_scroll_y = 0;
            self.patch_scroll_x = 0;
        }
    }

    fn selected_key(&self) -> Option<DiffSelectionKey> {
        self.selected_entry().map(selected_key)
    }

    fn selected_entry(&self) -> Option<&DiffFileStat> {
        self.visible_entries().get(self.selected)
    }

    fn visible_entries(&self) -> &[DiffFileStat] {
        &self.display_entries
    }

    fn effective_filters(&self) -> DiffFilterSpec {
        DiffFilterSpec {
            summary: DiffFilterSummary {
                scopes: self.scope_filter.as_summary_scopes(),
                path_patterns: self.base_filters.summary.path_patterns.clone(),
                exclude_risks: self.base_filters.summary.exclude_risks.clone(),
            },
            path_matcher: self.base_filters.path_matcher.clone(),
        }
    }

    fn rebuild_display_entries(&mut self) {
        let mut entries = self
            .report
            .as_ref()
            .map(|report| report.total.file_stats.clone())
            .unwrap_or_default();
        if self.grouping == DiffTuiGrouping::Category {
            entries.sort_by(|a, b| {
                review_category_rank(a)
                    .cmp(&review_category_rank(b))
                    .then_with(|| review_risk_rank(a).cmp(&review_risk_rank(b)))
                    .then_with(|| review_impact(b).cmp(&review_impact(a)))
                    .then_with(|| review_scope_rank(a).cmp(&review_scope_rank(b)))
                    .then_with(|| diff_status_rank(a.status).cmp(&diff_status_rank(b.status)))
                    .then_with(|| a.path.cmp(&b.path))
            });
        }
        self.display_entries = entries;
    }

    fn patch_render_view(&self) -> DiffPatchRenderView<'_> {
        build_patch_render_view(&self.patch_preview, self.show_patch_boilerplate)
    }
}

impl DiffTuiScopeFilter {
    fn all() -> Self {
        Self {
            unstaged: true,
            staged: true,
            untracked: true,
        }
    }

    fn from_initial(scopes: &[DiffScope]) -> Self {
        if scopes.is_empty() {
            return Self::all();
        }
        Self {
            unstaged: scopes.contains(&DiffScope::Unstaged),
            staged: scopes.contains(&DiffScope::Staged),
            untracked: scopes.contains(&DiffScope::Untracked),
        }
    }

    fn toggle(&mut self, scope: DiffScope) -> bool {
        let next = match scope {
            DiffScope::Unstaged => !self.unstaged,
            DiffScope::Staged => !self.staged,
            DiffScope::Untracked => !self.untracked,
        };
        if !next && self.enabled_count() == 1 {
            return false;
        }
        match scope {
            DiffScope::Unstaged => self.unstaged = next,
            DiffScope::Staged => self.staged = next,
            DiffScope::Untracked => self.untracked = next,
        }
        true
    }

    fn enabled_count(&self) -> usize {
        usize::from(self.unstaged) + usize::from(self.staged) + usize::from(self.untracked)
    }

    fn as_summary_scopes(&self) -> Vec<DiffScope> {
        if self.unstaged && self.staged && self.untracked {
            return Vec::new();
        }

        let mut scopes = Vec::new();
        if self.unstaged {
            scopes.push(DiffScope::Unstaged);
        }
        if self.staged {
            scopes.push(DiffScope::Staged);
        }
        if self.untracked {
            scopes.push(DiffScope::Untracked);
        }
        scopes
    }

    fn summary_label(&self) -> String {
        let mut labels = Vec::new();
        if self.unstaged {
            labels.push("unstaged");
        }
        if self.staged {
            labels.push("staged");
        }
        if self.untracked {
            labels.push("untracked");
        }
        if labels.len() == 3 {
            "all".to_string()
        } else {
            labels.join("+")
        }
    }
}

impl DiffTuiGrouping {
    fn label(self) -> &'static str {
        match self {
            Self::Flat => "flat",
            Self::Category => "category",
        }
    }
}

impl DiffTuiLayoutMode {
    fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Split => "split",
            Self::Stacked => "stacked",
            Self::PatchOnly => "patch-only",
            Self::FilesOnly => "files-only",
        }
    }

    fn resolve(self, area: Rect) -> DiffTuiResolvedLayout {
        match self {
            Self::Auto => {
                if area.width >= 78 {
                    DiffTuiResolvedLayout::Split
                } else if area.width >= 62 {
                    DiffTuiResolvedLayout::Stacked
                } else {
                    DiffTuiResolvedLayout::PatchOnly
                }
            }
            Self::Split => DiffTuiResolvedLayout::Split,
            Self::Stacked => DiffTuiResolvedLayout::Stacked,
            Self::PatchOnly => DiffTuiResolvedLayout::PatchOnly,
            Self::FilesOnly => DiffTuiResolvedLayout::FilesOnly,
        }
    }
}

impl From<&DiffFileStat> for DiffPatchCacheKey {
    fn from(value: &DiffFileStat) -> Self {
        Self {
            selection: selected_key(value),
            status: value.status,
            scopes: value.scopes.clone(),
            additions: value.additions,
            deletions: value.deletions,
            binary: value.binary,
        }
    }
}

fn draw_diff_tui(frame: &mut ratatui::Frame<'_>, app: &mut DiffTuiApp) {
    let layout = app.layout_mode.resolve(frame.area());
    let overview_height = overview_panel_height(
        app.last_refresh_error.is_some() || app.status_message.is_some(),
        frame.area(),
        layout,
    );
    let footer_height = footer_panel_height(frame.area(), layout);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(overview_height),
            Constraint::Min(12),
            Constraint::Length(footer_height),
        ])
        .split(frame.area());

    if overview_height > 0 {
        draw_overview(frame, app, chunks[0]);
    }
    draw_main(frame, app, chunks[1], layout);
    if footer_height > 0 {
        draw_footer(frame, app, chunks[2]);
    }

    if app.show_help {
        draw_help(frame);
    }
}

fn draw_overview(frame: &mut ratatui::Frame<'_>, app: &DiffTuiApp, area: Rect) {
    if area.height == 0 {
        return;
    }

    let Some(report) = app.report.as_ref() else {
        frame.render_widget(
            Paragraph::new(vec![Line::from(Span::styled(
                "loading workspace diff...",
                Style::default().fg(Color::DarkGray),
            ))])
            .alignment(Alignment::Left),
            area,
        );
        return;
    };

    let title = format!(
        "za diff tui  {} {} {}",
        app.repo_name,
        style_bullet(),
        report.head.as_deref().unwrap_or("(unborn)")
    );
    let visible_counts = format!(
        "visible {} {}  +{}  -{}",
        report.total.files,
        pluralize(report.total.files, "file", "files"),
        report.total.additions,
        report.total.deletions
    );
    let workspace_counts = format!(
        "workspace {} {}  +{}  -{}",
        report.workspace_total.files,
        pluralize(report.workspace_total.files, "file", "files"),
        report.workspace_total.additions,
        report.workspace_total.deletions
    );
    let scope_summary = render_scope_summary(report, false);
    let filter_summary = render_filter_summary(&report.filters, false);
    let attention_summary =
        render_attention_summary(&report.total.file_stats, &report.risk_policy, false);
    let freshness_line = format!("data {}", last_seen_label(app.last_refresh_at));
    let status_line = app
        .last_refresh_error
        .as_deref()
        .or(app.status_message.as_deref());
    let mut context_parts = vec![if scope_summary.is_empty() {
        "scope all".to_string()
    } else {
        scope_summary
    }];
    if !filter_summary.is_empty() {
        context_parts.push(filter_summary);
    }
    if !attention_summary.is_empty() {
        context_parts.push(attention_summary);
    }
    context_parts.push(freshness_line);

    let max_width = usize::from(area.width.saturating_sub(1));
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                truncate_end(&title, max_width.min(36)),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {}  ", style_bullet()),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw(truncate_end(
                &format!("{visible_counts}  {}  {workspace_counts}", style_bullet()),
                max_width.saturating_sub(40),
            )),
        ]),
        Line::from(Span::styled(
            truncate_end(
                &context_parts.join(&format!("  {}  ", style_bullet())),
                max_width,
            ),
            Style::default().fg(Color::DarkGray),
        )),
    ];
    if let Some(status_line) = status_line {
        lines.push(Line::from(Span::styled(
            truncate_end(status_line, max_width),
            if app.last_refresh_error.is_some() {
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            },
        )));
    }

    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Left), area);
}

fn draw_main(
    frame: &mut ratatui::Frame<'_>,
    app: &mut DiffTuiApp,
    area: Rect,
    layout: DiffTuiResolvedLayout,
) {
    match layout {
        DiffTuiResolvedLayout::Split => {
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints(split_layout_constraints(area.width))
                .split(area);
            draw_file_list(frame, app, chunks[0]);
            draw_detail(frame, app, chunks[1], layout);
        }
        DiffTuiResolvedLayout::Stacked => {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints(stacked_layout_constraints(area.height))
                .split(area);
            draw_file_list(frame, app, chunks[0]);
            draw_detail(frame, app, chunks[1], layout);
        }
        DiffTuiResolvedLayout::PatchOnly => draw_detail(frame, app, area, layout),
        DiffTuiResolvedLayout::FilesOnly => {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(1),
                    Constraint::Length(selection_panel_height(area, layout)),
                ])
                .split(area);
            draw_file_list(frame, app, chunks[0]);
            draw_selection_summary(frame, app, chunks[1], layout);
        }
    }
}

fn draw_file_list(frame: &mut ratatui::Frame<'_>, app: &mut DiffTuiApp, area: Rect) {
    let title = match app.focus {
        DiffTuiFocus::Files => "Files [focus]",
        DiffTuiFocus::Patch => "Files",
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);
    let width = chunks[1].width;
    let density = row_density(width);
    let (entry_count, scope_width, rows, items) = {
        let entries = app.visible_entries();
        let scope_width = compact_scope_width(entries);
        let rows = build_list_rows(entries, app.grouping);
        let max_total = entries
            .iter()
            .map(|entry| entry.additions.saturating_add(entry.deletions))
            .max()
            .unwrap_or_default();
        let items = if rows.is_empty() {
            vec![ListItem::new(Line::from(Span::styled(
                "No changes matched current filters.",
                Style::default().fg(Color::DarkGray),
            )))]
        } else {
            rows.iter()
                .map(|row| match row {
                    DiffListRow::Header(label) => ListItem::new(Line::from(Span::styled(
                        format!("{} {}", label, style_bullet()),
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::BOLD),
                    ))),
                    DiffListRow::Entry(index) => {
                        file_row_item(&entries[*index], width, scope_width, max_total, density)
                    }
                })
                .collect::<Vec<_>>()
        };
        (entries.len(), scope_width, rows, items)
    };
    let header = Paragraph::new(file_header_line(width, scope_width, density)).style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_widget(header, chunks[0]);
    let selected_entry =
        (entry_count > 0).then_some(app.selected.min(entry_count.saturating_sub(1)));
    let selected_row = selected_entry.and_then(|entry| selected_row_index(&rows, entry));
    let mut list_state = ListState::default()
        .with_offset(app.list_scroll_offset)
        .with_selected(selected_row);
    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, chunks[1], &mut list_state);
    app.list_scroll_offset = list_state.offset();
    app.list_viewport_rows = usize::from(chunks[1].height.max(1));
    if let Some(selected) = selected_entry {
        app.selected = selected;
    }

    let row_count = rows.len().max(1);
    let viewport_len = usize::from(chunks[1].height.max(1));
    let mut scrollbar_state = ScrollbarState::new(row_count)
        .position(list_state.offset())
        .viewport_content_length(viewport_len);
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .thumb_symbol("█")
        .track_symbol(Some("│"))
        .thumb_style(Style::default().fg(Color::Yellow))
        .track_style(Style::default().fg(Color::DarkGray));
    frame.render_stateful_widget(scrollbar, chunks[1], &mut scrollbar_state);
}

fn draw_detail(
    frame: &mut ratatui::Frame<'_>,
    app: &mut DiffTuiApp,
    area: Rect,
    layout: DiffTuiResolvedLayout,
) {
    let selection_height = selection_panel_height(area, layout);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(selection_height), Constraint::Min(1)])
        .split(area);
    draw_selection_summary(frame, app, chunks[0], layout);
    draw_patch_preview(frame, app, chunks[1]);
}

fn draw_selection_summary(
    frame: &mut ratatui::Frame<'_>,
    app: &DiffTuiApp,
    area: Rect,
    layout: DiffTuiResolvedLayout,
) {
    let block = Block::default().borders(Borders::ALL).title("Selection");
    let lines = app
        .selected_entry()
        .map(|entry| selection_summary_lines(app, entry, area, layout))
        .unwrap_or_else(empty_selection_summary_lines);
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_patch_preview(frame: &mut ratatui::Frame<'_>, app: &mut DiffTuiApp, area: Rect) {
    let title = match app.focus {
        DiffTuiFocus::Patch => "Patch [focus]",
        DiffTuiFocus::Files => "Patch",
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let inner_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);
    let viewport_rows = usize::from(inner_chunks[1].height.max(1));
    let viewport_cols = usize::from(inner_chunks[1].width.max(1));
    let (line_count, max_scroll_x, rendered_lines) = {
        let view = app.patch_render_view();
        let line_count = view.visible_lines.len().max(1);
        let max_scroll_x = view
            .visible_lines
            .iter()
            .map(|line| line.text.chars().count())
            .max()
            .unwrap_or_default()
            .saturating_sub(viewport_cols);
        let rendered_lines = view
            .visible_lines
            .iter()
            .map(|line| render_patch_line(line))
            .collect::<Vec<_>>();
        (line_count, max_scroll_x, rendered_lines)
    };
    app.patch_viewport_rows = viewport_rows;
    app.patch_viewport_cols = viewport_cols;

    let max_scroll_y = line_count.saturating_sub(viewport_rows);
    app.patch_scroll_y = app.patch_scroll_y.min(max_scroll_y);
    app.patch_scroll_x = app.patch_scroll_x.min(max_scroll_x);
    let sticky = patch_sticky_line(
        &app.patch_preview,
        &app.patch_render_view(),
        app.patch_scroll_y,
        app.show_patch_boilerplate,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_end(
                &sticky,
                usize::from(inner_chunks[0].width.saturating_sub(1)),
            ),
            Style::default().fg(Color::DarkGray),
        ))),
        inner_chunks[0],
    );
    let paragraph = Paragraph::new(rendered_lines).scroll((
        app.patch_scroll_y.min(u16::MAX as usize) as u16,
        app.patch_scroll_x.min(u16::MAX as usize) as u16,
    ));
    frame.render_widget(paragraph, inner_chunks[1]);

    let mut scrollbar_state = ScrollbarState::new(line_count)
        .position(app.patch_scroll_y)
        .viewport_content_length(viewport_rows);
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .thumb_symbol("█")
        .track_symbol(Some("│"))
        .thumb_style(Style::default().fg(Color::Yellow))
        .track_style(Style::default().fg(Color::DarkGray));
    frame.render_stateful_widget(scrollbar, inner_chunks[1], &mut scrollbar_state);
}

fn draw_footer(frame: &mut ratatui::Frame<'_>, app: &DiffTuiApp, area: Rect) {
    if area.height == 0 {
        return;
    }

    let focus_label = match app.focus {
        DiffTuiFocus::Files => "files",
        DiffTuiFocus::Patch => "patch",
    };
    let hint = format!(
        "? help  {} focus  j/k move  Tab switch  u/s/n scope  [/] risk  {{}} hunk  c/v/m modes  q quit",
        focus_label
    );
    frame.render_widget(
        Paragraph::new(vec![Line::from(Span::styled(
            truncate_end(&hint, usize::from(area.width.saturating_sub(1))),
            Style::default().fg(Color::DarkGray),
        ))]),
        area,
    );
}

fn draw_help(frame: &mut ratatui::Frame<'_>) {
    let area = centered_rect(74, 64, frame.area());
    let lines = vec![
        Line::from(Span::styled(
            "Continuous diff review dashboard",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("files"),
        Line::from("  j/k or Up/Down move selection"),
        Line::from("  g/G or Home/End jump to top/bottom"),
        Line::from("  [/ ] jump to previous/next file carrying review risk"),
        Line::from("  c toggles flat/category grouping"),
        Line::from("  v cycles auto/split/stacked/patch-only/files-only"),
        Line::from(""),
        Line::from("patch"),
        Line::from("  Tab or Enter switches focus between file queue and patch"),
        Line::from("  PgUp/PgDn page the active pane"),
        Line::from("  { / } jump between hunks"),
        Line::from("  h/l or Left/Right scroll patch horizontally"),
        Line::from("  m toggles compact/full patch metadata"),
        Line::from("  0 resets horizontal patch scroll"),
        Line::from(""),
        Line::from("filters"),
        Line::from("  u toggles unstaged"),
        Line::from("  s toggles staged"),
        Line::from("  n toggles untracked/new"),
        Line::from("  a resets scope filter to all"),
        Line::from(""),
        Line::from("general"),
        Line::from("  r refreshes immediately"),
        Line::from("  ? or Esc closes help"),
        Line::from("  q quits"),
    ];
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title("Help")),
        area,
    );
}

fn overview_panel_height(has_status: bool, area: Rect, layout: DiffTuiResolvedLayout) -> u16 {
    let compact_height = if has_status { 3 } else { 2 };
    match layout {
        DiffTuiResolvedLayout::PatchOnly if area.height <= 28 || area.width < 96 => compact_height,
        _ if area.height <= 28 => compact_height,
        _ if area.width < 112 => compact_height,
        _ => {
            if has_status {
                3
            } else {
                2
            }
        }
    }
}

fn footer_panel_height(area: Rect, layout: DiffTuiResolvedLayout) -> u16 {
    match layout {
        DiffTuiResolvedLayout::PatchOnly if area.height <= 18 => 0,
        _ if area.height <= 18 => 0,
        _ => 1,
    }
}

fn split_layout_constraints(width: u16) -> [Constraint; 2] {
    if width >= 140 {
        [Constraint::Percentage(30), Constraint::Percentage(70)]
    } else if width >= 110 {
        [Constraint::Percentage(34), Constraint::Percentage(66)]
    } else if width >= 92 {
        [Constraint::Percentage(38), Constraint::Percentage(62)]
    } else {
        [Constraint::Percentage(40), Constraint::Percentage(60)]
    }
}

fn stacked_layout_constraints(height: u16) -> [Constraint; 2] {
    if height <= 14 {
        [Constraint::Percentage(32), Constraint::Percentage(68)]
    } else {
        [Constraint::Percentage(36), Constraint::Percentage(64)]
    }
}

fn file_header_line(width: u16, scope_width: usize, density: DiffTuiRowDensity) -> Line<'static> {
    let badge_width = badge_width(width, density);
    let show_risk = shows_risk_marker(density);
    let show_scope = shows_scope(density);
    let show_counts = shows_counts(density);
    let show_stat = shows_stat(density);
    let path_width = file_path_width(
        width,
        scope_width,
        badge_width,
        show_risk,
        show_scope,
        show_counts,
        show_stat,
    );
    let mut spans = vec![Span::styled("st", Style::default().fg(Color::DarkGray))];
    if show_risk {
        spans.push(Span::raw(" "));
        spans.push(Span::styled("!", Style::default().fg(Color::DarkGray)));
    }
    if badge_width > 0 {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("{:<width$}", "tag", width = badge_width),
            Style::default().fg(Color::DarkGray),
        ));
    }
    if show_scope {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("{:<width$}", "sc", width = scope_width),
            Style::default().fg(Color::DarkGray),
        ));
    }
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        format!("{:<width$}", "file", width = path_width),
        Style::default().fg(Color::DarkGray),
    ));
    if show_counts {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("{:>6}", "+add"),
            Style::default().fg(Color::Green),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("{:>6}", "-del"),
            Style::default().fg(Color::Red),
        ));
    }
    if show_stat {
        spans.push(Span::raw(" "));
        spans.push(Span::styled("stat", Style::default().fg(Color::DarkGray)));
    }
    Line::from(spans)
}

fn file_row_item(
    entry: &DiffFileStat,
    width: u16,
    scope_width: usize,
    max_total: u64,
    density: DiffTuiRowDensity,
) -> ListItem<'static> {
    let scope_width = scope_width.max(2);
    let badge_width = badge_width(width, density);
    let show_risk = shows_risk_marker(density);
    let show_scope = shows_scope(density);
    let show_counts = shows_counts(density);
    let show_stat = shows_stat(density);
    let path_width = file_path_width(
        width,
        scope_width,
        badge_width,
        show_risk,
        show_scope,
        show_counts,
        show_stat,
    );
    let scope_label = review_scope_label(entry, DiffScopeLabelMode::Compact);
    let path_label = review_path_plain_with_width(entry, path_width);

    let mut spans = vec![
        Span::styled(
            format!("{:>2}", entry.status.short_label()),
            tui_status_style(entry.status),
        ),
        Span::raw(" "),
    ];
    if show_risk {
        spans.push(Span::styled(
            highest_risk_level(entry).map(risk_marker).unwrap_or(" "),
            tui_risk_style(highest_risk_level(entry)),
        ));
        spans.push(Span::raw(" "));
    }
    if badge_width > 0 {
        let (badge_label, badge_style) = primary_badge(entry);
        spans.push(Span::styled(
            format!("{badge_label:<width$}", width = badge_width),
            badge_style,
        ));
        spans.push(Span::raw(" "));
    }
    if show_scope {
        spans.push(Span::styled(
            format!("{scope_label:<width$}", width = scope_width),
            tui_scope_style(entry),
        ));
        spans.push(Span::raw(" "));
    }
    spans.push(Span::raw(format!(
        "{path_label:<width$}",
        width = path_width
    )));
    if show_counts {
        let addition_label = if entry.binary {
            "binary".to_string()
        } else {
            format!("+{}", entry.additions)
        };
        let deletion_label = if entry.binary {
            "-".to_string()
        } else {
            format!("-{}", entry.deletions)
        };
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("{addition_label:>6}"),
            tui_add_style(),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("{deletion_label:>6}"),
            tui_del_style(),
        ));
    }
    if show_stat {
        spans.push(Span::raw(" "));
        spans.extend(diff_stat_spans(
            entry,
            max_total,
            unicode_diff_stat_enabled(),
        ));
    }
    ListItem::new(Line::from(spans))
}

fn selection_summary_lines(
    app: &DiffTuiApp,
    entry: &DiffFileStat,
    area: Rect,
    layout: DiffTuiResolvedLayout,
) -> Vec<Line<'static>> {
    let width = usize::from(area.width.saturating_sub(4)).max(20);
    let index = app.selected.saturating_add(1);
    let total = app.visible_entries().len();
    let compact = area.height <= 5 || area.width < 52;
    let minimal = compact
        && (matches!(
            layout,
            DiffTuiResolvedLayout::Split | DiffTuiResolvedLayout::PatchOnly
        ));
    let risk_summary = if entry.risks.is_empty() {
        "risk none".to_string()
    } else {
        let labels = entry
            .risks
            .iter()
            .take(if compact { 2 } else { 3 })
            .map(|risk| risk_label(risk, app.report.as_ref().map(|report| &report.risk_policy)))
            .collect::<Vec<_>>()
            .join(" ");
        format!("risk {labels}")
    };

    if minimal {
        let detail = if let Some(previous_path) = &entry.previous_path {
            format!(
                "from {}",
                truncate_middle(previous_path, width.saturating_sub(5))
            )
        } else {
            format!(
                "preview {}",
                patch_loaded_label(app.patch_preview.loaded_at)
            )
        };
        return vec![
            Line::from(vec![
                Span::styled(
                    truncate_middle(&entry.path, width.saturating_sub(8)),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {} {index}/{total}", style_bullet()),
                    Style::default().fg(Color::DarkGray),
                ),
            ]),
            Line::from(Span::styled(
                truncate_end(
                    &format!(
                        "{}  {}  {}  {}  impact {}",
                        entry.status.short_label(),
                        review_scope_label(entry, DiffScopeLabelMode::Compact),
                        review_category_label(entry),
                        style_bullet(),
                        review_impact(entry)
                    ),
                    width,
                ),
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                truncate_end(
                    &format!("{risk_summary}  {}  {detail}", style_bullet()),
                    width,
                ),
                Style::default().fg(Color::DarkGray),
            )),
        ];
    }

    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                truncate_middle(&entry.path, width),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {} {index}/{total}", style_bullet()),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(vec![
            Span::styled(entry.status.short_label(), tui_status_style(entry.status)),
            Span::raw("  "),
            Span::styled(
                review_scope_label(
                    entry,
                    if compact {
                        DiffScopeLabelMode::Compact
                    } else {
                        DiffScopeLabelMode::Full
                    },
                ),
                tui_scope_style(entry),
            ),
            Span::styled(
                format!("  {}  {}", style_bullet(), review_category_label(entry)),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(vec![
            Span::styled(format!("+{}", entry.additions), tui_add_style()),
            Span::raw("  "),
            Span::styled(format!("-{}", entry.deletions), tui_del_style()),
            Span::styled(
                format!("  {}  impact {}", style_bullet(), review_impact(entry)),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
    ];

    lines.push(Line::from(Span::styled(
        truncate_end(&risk_summary, width),
        Style::default().fg(Color::DarkGray),
    )));

    if !compact {
        if let Some(previous_path) = &entry.previous_path {
            lines.push(Line::from(vec![
                Span::styled("from", Style::default().fg(Color::DarkGray)),
                Span::raw(" "),
                Span::raw(truncate_middle(previous_path, width.saturating_sub(5))),
            ]));
        } else {
            lines.push(Line::from(Span::styled(
                format!(
                    "preview {}",
                    patch_loaded_label(app.patch_preview.loaded_at)
                ),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }

    lines
}

fn empty_selection_summary_lines() -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(
            "No file selected.",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("Use j/k to move through the file queue."),
        Line::from("Use Tab to switch focus into the patch pane."),
        Line::from(Span::styled(
            "The dashboard refreshes automatically.",
            Style::default().fg(Color::DarkGray),
        )),
    ]
}

fn render_patch_line(line: &DiffPatchLine) -> Line<'static> {
    let style = match line.kind {
        DiffPatchLineKind::Section => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        DiffPatchLineKind::MetaInfo => Style::default().fg(Color::Blue),
        DiffPatchLineKind::MetaBoilerplate => Style::default().fg(Color::DarkGray),
        DiffPatchLineKind::Hunk => Style::default().fg(Color::Cyan),
        DiffPatchLineKind::Addition => tui_add_style(),
        DiffPatchLineKind::Deletion => tui_del_style(),
        DiffPatchLineKind::Plain => Style::default(),
        DiffPatchLineKind::Dim => Style::default().fg(Color::DarkGray),
        DiffPatchLineKind::Error => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
    };
    Line::from(Span::styled(line.text.clone(), style))
}

fn load_patch_preview(repo_root: &Path, entry: &DiffFileStat) -> Result<DiffPatchPreview> {
    let mut lines = Vec::new();
    for (index, spec) in patch_specs(entry).into_iter().enumerate() {
        if index > 0 {
            lines.push(DiffPatchLine {
                text: String::new(),
                kind: DiffPatchLineKind::Plain,
            });
        }
        lines.push(DiffPatchLine {
            text: format!("[{}]", spec.scope.label()),
            kind: DiffPatchLineKind::Section,
        });
        let output = git_output_dynamic(repo_root, &spec.args, spec.allowed_codes)?;
        let stdout = String::from_utf8_lossy(&output.stdout).replace('\r', "");
        if stdout.trim().is_empty() {
            lines.push(DiffPatchLine {
                text: "(no textual patch for this scope)".to_string(),
                kind: DiffPatchLineKind::Dim,
            });
        } else {
            lines.extend(stdout.lines().map(classify_patch_line));
        }
    }

    if lines.is_empty() {
        lines.push(DiffPatchLine {
            text: "No patch preview available.".to_string(),
            kind: DiffPatchLineKind::Dim,
        });
    }

    Ok(DiffPatchPreview {
        lines,
        loaded_at: Some(SystemTime::now()),
    })
}

fn patch_specs(entry: &DiffFileStat) -> Vec<DiffPatchSpec> {
    let mut specs = Vec::new();
    if entry.scopes.contains(&DiffScope::Unstaged) {
        specs.push(DiffPatchSpec {
            scope: DiffScope::Unstaged,
            args: build_git_patch_args(entry, DiffScope::Unstaged),
            allowed_codes: &[0],
        });
    }
    if entry.scopes.contains(&DiffScope::Staged) {
        specs.push(DiffPatchSpec {
            scope: DiffScope::Staged,
            args: build_git_patch_args(entry, DiffScope::Staged),
            allowed_codes: &[0],
        });
    }
    if entry.scopes.contains(&DiffScope::Untracked) {
        specs.push(DiffPatchSpec {
            scope: DiffScope::Untracked,
            args: build_git_patch_args(entry, DiffScope::Untracked),
            allowed_codes: &[1],
        });
    }
    specs
}

fn build_git_patch_args(entry: &DiffFileStat, scope: DiffScope) -> Vec<String> {
    match scope {
        DiffScope::Unstaged | DiffScope::Staged => {
            let mut args = vec![
                "diff".to_string(),
                "--no-ext-diff".to_string(),
                "--no-color".to_string(),
                "--unified=3".to_string(),
                "-M".to_string(),
            ];
            if scope == DiffScope::Staged {
                args.push("--cached".to_string());
            }
            args.push("--".to_string());
            args.extend(patch_paths(entry));
            args
        }
        DiffScope::Untracked => vec![
            "diff".to_string(),
            "--no-ext-diff".to_string(),
            "--no-color".to_string(),
            "--unified=3".to_string(),
            "--no-index".to_string(),
            "--".to_string(),
            "/dev/null".to_string(),
            entry.path.clone(),
        ],
    }
}

fn patch_paths(entry: &DiffFileStat) -> Vec<String> {
    let mut paths = Vec::new();
    if let Some(previous_path) = &entry.previous_path {
        paths.push(previous_path.clone());
    }
    if paths.last() != Some(&entry.path) {
        paths.push(entry.path.clone());
    }
    paths
}

fn classify_patch_line(line: &str) -> DiffPatchLine {
    let kind = if line.starts_with("diff --git")
        || line.starts_with("index ")
        || line.starts_with("--- ")
        || line.starts_with("+++ ")
    {
        DiffPatchLineKind::MetaBoilerplate
    } else if line.starts_with("rename from ")
        || line.starts_with("rename to ")
        || line.starts_with("new file mode ")
        || line.starts_with("deleted file mode ")
        || line.starts_with("Binary files ")
    {
        DiffPatchLineKind::MetaInfo
    } else if line.starts_with("@@") {
        DiffPatchLineKind::Hunk
    } else if line.starts_with('+') && !line.starts_with("+++") {
        DiffPatchLineKind::Addition
    } else if line.starts_with('-') && !line.starts_with("---") {
        DiffPatchLineKind::Deletion
    } else if line.is_empty() {
        DiffPatchLineKind::Dim
    } else {
        DiffPatchLineKind::Plain
    };
    DiffPatchLine {
        text: line.to_string(),
        kind,
    }
}

fn build_list_rows(
    entries: &[DiffFileStat],
    grouping: DiffTuiGrouping,
) -> Vec<DiffListRow<'static>> {
    let mut rows = Vec::new();
    let mut current_group: Option<&'static str> = None;
    for (index, entry) in entries.iter().enumerate() {
        let group =
            (grouping == DiffTuiGrouping::Category).then_some(review_category_group_label(entry));
        if let Some(group) = group
            && current_group != Some(group)
        {
            rows.push(DiffListRow::Header(group));
            current_group = Some(group);
        }
        rows.push(DiffListRow::Entry(index));
    }
    rows
}

fn selected_row_index(rows: &[DiffListRow<'_>], selected_entry: usize) -> Option<usize> {
    rows.iter()
        .position(|row| matches!(row, DiffListRow::Entry(index) if *index == selected_entry))
}

fn row_density(width: u16) -> DiffTuiRowDensity {
    match width {
        0..=33 => DiffTuiRowDensity::Minimal,
        34..=57 => DiffTuiRowDensity::Compact,
        _ => DiffTuiRowDensity::Wide,
    }
}

fn selection_panel_height(area: Rect, layout: DiffTuiResolvedLayout) -> u16 {
    match layout {
        DiffTuiResolvedLayout::PatchOnly => 4,
        DiffTuiResolvedLayout::Split if area.height <= 12 || area.width < 56 => 4,
        DiffTuiResolvedLayout::Split => 5,
        DiffTuiResolvedLayout::Stacked if area.height <= 14 => 4,
        DiffTuiResolvedLayout::Stacked => 5,
        DiffTuiResolvedLayout::FilesOnly if area.height <= 10 => 4,
        DiffTuiResolvedLayout::FilesOnly => DIFF_TUI_SELECTION_HEIGHT,
    }
}

fn badge_width(width: u16, density: DiffTuiRowDensity) -> usize {
    if density == DiffTuiRowDensity::Wide && width >= 60 {
        5
    } else {
        0
    }
}

fn shows_risk_marker(density: DiffTuiRowDensity) -> bool {
    !matches!(density, DiffTuiRowDensity::Minimal)
}

fn shows_scope(density: DiffTuiRowDensity) -> bool {
    !matches!(density, DiffTuiRowDensity::Minimal)
}

fn shows_counts(_density: DiffTuiRowDensity) -> bool {
    true
}

fn shows_stat(density: DiffTuiRowDensity) -> bool {
    matches!(density, DiffTuiRowDensity::Wide)
}

fn file_path_width(
    width: u16,
    scope_width: usize,
    badge_width: usize,
    show_risk: bool,
    show_scope: bool,
    show_counts: bool,
    show_stat: bool,
) -> usize {
    let fixed = 2
        + 1
        + if show_risk { 1 + 1 } else { 0 }
        + if badge_width > 0 { 1 + badge_width } else { 0 }
        + if show_scope { 1 + scope_width } else { 0 }
        + if show_counts { 1 + 6 + 1 + 6 } else { 0 }
        + if show_stat { 1 + 5 } else { 0 };
    usize::from(width).saturating_sub(fixed).clamp(1, 64)
}

fn primary_badge(entry: &DiffFileStat) -> (&'static str, Style) {
    if entry.binary {
        return (
            "bin",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    }
    if matches!(entry.status, DiffStatus::Renamed | DiffStatus::Copied) {
        return ("ren", Style::default().fg(Color::Cyan));
    }
    if entry
        .risks
        .iter()
        .any(|risk| matches!(risk.kind, DiffRiskKind::Large))
    {
        return (
            "big",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        );
    }
    if entry
        .risks
        .iter()
        .any(|risk| matches!(risk.kind, DiffRiskKind::Ci))
    {
        return (
            "ci",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    }
    if entry
        .risks
        .iter()
        .any(|risk| matches!(risk.kind, DiffRiskKind::Config))
    {
        return ("cfg", Style::default().fg(Color::Magenta));
    }
    if entry
        .risks
        .iter()
        .any(|risk| matches!(risk.kind, DiffRiskKind::Lockfile))
    {
        return ("lock", Style::default().fg(Color::Blue));
    }
    if entry
        .risks
        .iter()
        .any(|risk| matches!(risk.kind, DiffRiskKind::Generated))
    {
        return ("gen", Style::default().fg(Color::DarkGray));
    }
    ("", Style::default().fg(Color::DarkGray))
}

fn review_category_group_label(entry: &DiffFileStat) -> &'static str {
    let path = entry.path.to_ascii_lowercase();
    if is_ci_like_path(&path) || is_config_like_path(&path) {
        "Config & CI"
    } else if is_source_like_path(&path) {
        "Source"
    } else if is_test_like_path(&path) {
        "Tests"
    } else if is_doc_like_path(&path) {
        "Docs"
    } else if entry
        .risks
        .iter()
        .any(|risk| matches!(risk.kind, DiffRiskKind::Binary))
    {
        "Binary"
    } else if entry
        .risks
        .iter()
        .any(|risk| matches!(risk.kind, DiffRiskKind::Generated))
    {
        "Generated"
    } else {
        "Other"
    }
}

fn build_patch_render_view<'a>(
    preview: &'a DiffPatchPreview,
    show_patch_boilerplate: bool,
) -> DiffPatchRenderView<'a> {
    let mut view = DiffPatchRenderView::default();
    for (raw_index, line) in preview.lines.iter().enumerate() {
        if !show_patch_boilerplate && line.kind == DiffPatchLineKind::MetaBoilerplate {
            continue;
        }
        if line.kind == DiffPatchLineKind::Hunk {
            view.hunk_rows.push(view.visible_lines.len());
        }
        view.raw_indices.push(raw_index);
        view.visible_lines.push(line);
    }
    view
}

fn patch_sticky_line(
    preview: &DiffPatchPreview,
    view: &DiffPatchRenderView<'_>,
    scroll_y: usize,
    show_patch_boilerplate: bool,
) -> String {
    let current_raw_index = view
        .raw_indices
        .get(scroll_y.min(view.raw_indices.len().saturating_sub(1)))
        .copied()
        .unwrap_or_default();
    let mut section = None;
    let mut hunk = None;
    for line in preview
        .lines
        .iter()
        .take(current_raw_index.saturating_add(1))
    {
        match line.kind {
            DiffPatchLineKind::Section => section = Some(line.text.as_str()),
            DiffPatchLineKind::Hunk => hunk = Some(line.text.as_str()),
            _ => {}
        }
    }
    let section = section.unwrap_or("[workspace]");
    let hunk = hunk.unwrap_or_else(|| {
        preview
            .lines
            .iter()
            .skip(current_raw_index)
            .find(|line| line.kind == DiffPatchLineKind::Hunk)
            .map(|line| line.text.as_str())
            .unwrap_or("@@ no hunk @@")
    });
    format!(
        "{section}  {}  {}  {}  meta {}",
        style_bullet(),
        truncate_end(hunk, 40),
        style_bullet(),
        if show_patch_boilerplate {
            "full"
        } else {
            "compact"
        }
    )
}

fn last_seen_label(timestamp: Option<SystemTime>) -> String {
    match timestamp {
        Some(value) => human_age(value),
        None => "pending".to_string(),
    }
}

fn workspace_signature(repo_root: &Path) -> Result<Vec<u8>> {
    let output = git_output(
        repo_root,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
            "--",
        ],
    )?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("`git status --porcelain=v1 -z` failed: {}", stderr.trim());
    }
    Ok(output.stdout)
}

fn selected_key(entry: &DiffFileStat) -> DiffSelectionKey {
    DiffSelectionKey {
        path: entry.path.clone(),
        previous_path: entry.previous_path.clone(),
    }
}

fn compact_scope_width(entries: &[DiffFileStat]) -> usize {
    entries
        .iter()
        .map(|entry| review_scope_label(entry, DiffScopeLabelMode::Compact))
        .map(|scope| scope.chars().count())
        .max()
        .unwrap_or(2)
        .max(2)
}

fn diff_stat_spans(
    entry: &DiffFileStat,
    max_total: u64,
    use_unicode_stat: bool,
) -> Vec<Span<'static>> {
    let total = entry.additions.saturating_add(entry.deletions);
    if entry.binary {
        return vec![Span::styled("bin", Style::default().fg(Color::Cyan))];
    }
    if total == 0 || max_total == 0 {
        return vec![Span::styled(
            empty_glyph(use_unicode_stat).repeat(DIFF_STAT_BLOCK_COUNT),
            Style::default().fg(Color::DarkGray),
        )];
    }

    let mut bar_width =
        ((total as f64 / max_total as f64) * DIFF_STAT_BLOCK_COUNT as f64).round() as usize;
    if bar_width == 0 {
        bar_width = 1;
    }
    bar_width = bar_width.min(DIFF_STAT_BLOCK_COUNT);

    let mut add_width =
        ((entry.additions as f64 / total as f64) * bar_width as f64).round() as usize;
    add_width = add_width.min(bar_width);
    let mut del_width = bar_width.saturating_sub(add_width);
    if bar_width > 1 && entry.additions > 0 && add_width == 0 {
        add_width = 1;
        del_width = bar_width.saturating_sub(add_width);
    }
    if bar_width > 1 && entry.deletions > 0 && del_width == 0 {
        del_width = 1;
        add_width = bar_width.saturating_sub(del_width);
    }

    let filled = filled_glyph(use_unicode_stat);
    let empty = empty_glyph(use_unicode_stat);
    let mut spans = Vec::new();
    if add_width > 0 {
        spans.push(Span::styled(
            filled.repeat(add_width),
            Style::default().fg(Color::Green),
        ));
    }
    if del_width > 0 {
        spans.push(Span::styled(
            filled.repeat(del_width),
            Style::default().fg(Color::Red),
        ));
    }
    let empty_width = DIFF_STAT_BLOCK_COUNT.saturating_sub(bar_width);
    if empty_width > 0 {
        spans.push(Span::styled(
            empty.repeat(empty_width),
            Style::default().fg(Color::DarkGray),
        ));
    }
    spans
}

fn filled_glyph(use_unicode_stat: bool) -> &'static str {
    if use_unicode_stat {
        DIFF_STAT_FILLED_BLOCK
    } else {
        "+"
    }
}

fn empty_glyph(use_unicode_stat: bool) -> &'static str {
    if use_unicode_stat {
        DIFF_STAT_EMPTY_BLOCK
    } else {
        "."
    }
}

fn review_category_label(entry: &DiffFileStat) -> &'static str {
    let path = entry.path.to_ascii_lowercase();
    if is_ci_like_path(&path) {
        "ci"
    } else if is_config_like_path(&path) {
        "config"
    } else if is_source_like_path(&path) {
        "source"
    } else if is_test_like_path(&path) {
        "test"
    } else if is_doc_like_path(&path) {
        "docs"
    } else if entry
        .risks
        .iter()
        .any(|risk| matches!(risk.kind, DiffRiskKind::Generated))
    {
        "generated"
    } else if entry
        .risks
        .iter()
        .any(|risk| matches!(risk.kind, DiffRiskKind::Binary))
    {
        "binary"
    } else {
        "other"
    }
}

fn risk_label(risk: &DiffRisk, risk_policy: Option<&DiffRiskPolicy>) -> String {
    match risk.kind {
        DiffRiskKind::Large => risk_policy
            .map(|policy| format!("large>={}", policy.large_threshold))
            .unwrap_or_else(|| "large".to_string()),
        _ => risk.kind.label().to_string(),
    }
}

fn tui_status_style(status: DiffStatus) -> Style {
    match status {
        DiffStatus::Added => Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
        DiffStatus::Deleted => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        DiffStatus::Renamed | DiffStatus::Copied => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        DiffStatus::Modified => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        DiffStatus::TypeChanged => Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD),
        DiffStatus::Unmerged => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        DiffStatus::Untracked => Style::default()
            .fg(Color::Blue)
            .add_modifier(Modifier::BOLD),
        DiffStatus::Unknown => Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    }
}

fn tui_scope_style(entry: &DiffFileStat) -> Style {
    if entry.scopes.contains(&DiffScope::Unstaged) {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::DIM)
    } else if entry.scopes.contains(&DiffScope::Staged) {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::DIM)
    } else {
        Style::default().fg(Color::Blue).add_modifier(Modifier::DIM)
    }
}

fn tui_risk_style(level: Option<DiffRiskLevel>) -> Style {
    match level {
        Some(DiffRiskLevel::High) => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        Some(DiffRiskLevel::Medium) => Style::default().fg(Color::Magenta),
        None => Style::default().fg(Color::DarkGray),
    }
}

fn tui_add_style() -> Style {
    Style::default().fg(Color::Green)
}

fn tui_del_style() -> Style {
    Style::default().fg(Color::Red)
}

fn style_bullet() -> &'static str {
    "·"
}

fn patch_loaded_label(loaded_at: Option<SystemTime>) -> String {
    match loaded_at {
        Some(value) => format!("loaded {}", human_age(value)),
        None => "loaded just now".to_string(),
    }
}

fn human_age(timestamp: SystemTime) -> String {
    let now = SystemTime::now();
    let seconds = now.duration_since(timestamp).unwrap_or_default().as_secs();
    match seconds {
        0 => "just now".to_string(),
        1..=59 => format!("{seconds}s ago"),
        60..=3599 => format!("{}m ago", seconds / 60),
        _ => format!("{}h ago", seconds / 3600),
    }
}

fn truncate_end(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_string();
    }
    if width <= 1 {
        return "…".to_string();
    }
    let prefix = value
        .chars()
        .take(width.saturating_sub(1))
        .collect::<String>();
    format!("{prefix}…")
}

fn truncate_middle(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_string();
    }
    if width <= 1 {
        return "…".to_string();
    }
    let lead = width.saturating_sub(1) / 2;
    let tail = width.saturating_sub(lead + 1);
    let prefix = value.chars().take(lead).collect::<String>();
    let suffix = value
        .chars()
        .rev()
        .take(tail)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    format!("{prefix}…{suffix}")
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn git_output_dynamic(cwd: &Path, args: &[String], allowed_codes: &[i32]) -> Result<Output> {
    match Command::new("git")
        .args(args.iter().map(String::as_str))
        .current_dir(cwd)
        .output()
    {
        Ok(output)
            if output.status.success()
                || output
                    .status
                    .code()
                    .is_some_and(|code| allowed_codes.contains(&code)) =>
        {
            Ok(output)
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("`git {}` failed: {}", args.join(" "), stderr.trim())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            bail!("`za diff --tui` requires `git`; install it first")
        }
        Err(err) => Err(err).with_context(|| format!("run `git {}`", args.join(" "))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn scope_filter_defaults_to_all_when_no_cli_scope_is_set() {
        let scope_filter = DiffTuiScopeFilter::from_initial(&[]);
        assert!(scope_filter.unstaged);
        assert!(scope_filter.staged);
        assert!(scope_filter.untracked);
        assert_eq!(scope_filter.as_summary_scopes(), Vec::<DiffScope>::new());
    }

    #[test]
    fn scope_filter_refuses_to_hide_last_remaining_scope() {
        let mut scope_filter = DiffTuiScopeFilter {
            unstaged: true,
            staged: false,
            untracked: false,
        };
        assert!(!scope_filter.toggle(DiffScope::Unstaged));
        assert!(scope_filter.unstaged);
    }

    #[test]
    fn patch_paths_include_rename_source_before_destination() {
        let entry = DiffFileStat {
            path: "src/new.rs".to_string(),
            previous_path: Some("src/old.rs".to_string()),
            renamed_from: None,
            renamed_to: None,
            additions: 1,
            deletions: 1,
            binary: false,
            status: DiffStatus::Renamed,
            primary_scope: Some(DiffScope::Staged),
            scopes: vec![DiffScope::Staged],
            risks: Vec::new(),
        };
        assert_eq!(patch_paths(&entry), vec!["src/old.rs", "src/new.rs"]);
    }

    #[test]
    fn classify_patch_line_marks_additions_and_deletions() {
        assert_eq!(
            classify_patch_line("+hello").kind,
            DiffPatchLineKind::Addition
        );
        assert_eq!(
            classify_patch_line("-hello").kind,
            DiffPatchLineKind::Deletion
        );
        assert_eq!(
            classify_patch_line("@@ -1 +1 @@").kind,
            DiffPatchLineKind::Hunk
        );
        assert_eq!(
            classify_patch_line("diff --git a/x b/x").kind,
            DiffPatchLineKind::MetaBoilerplate
        );
    }

    #[test]
    fn build_patch_render_view_hides_boilerplate_in_compact_mode() {
        let preview = DiffPatchPreview {
            lines: vec![
                classify_patch_line("diff --git a/x b/x"),
                classify_patch_line("@@ -1 +1 @@"),
                classify_patch_line("+hello"),
            ],
            loaded_at: None,
        };
        let view = build_patch_render_view(&preview, false);
        assert_eq!(view.visible_lines.len(), 2);
        assert_eq!(view.hunk_rows, vec![0]);
    }

    #[test]
    fn build_list_rows_inserts_category_headers() {
        let entries = vec![
            DiffFileStat {
                path: "Cargo.toml".to_string(),
                previous_path: None,
                renamed_from: None,
                renamed_to: None,
                additions: 1,
                deletions: 0,
                binary: false,
                status: DiffStatus::Modified,
                primary_scope: Some(DiffScope::Unstaged),
                scopes: vec![DiffScope::Unstaged],
                risks: vec![DiffRisk {
                    kind: DiffRiskKind::Config,
                    level: DiffRiskLevel::Medium,
                }],
            },
            DiffFileStat {
                path: "src/main.rs".to_string(),
                previous_path: None,
                renamed_from: None,
                renamed_to: None,
                additions: 1,
                deletions: 0,
                binary: false,
                status: DiffStatus::Modified,
                primary_scope: Some(DiffScope::Unstaged),
                scopes: vec![DiffScope::Unstaged],
                risks: Vec::new(),
            },
        ];
        let rows = build_list_rows(&entries, DiffTuiGrouping::Category);
        assert!(matches!(
            rows.first(),
            Some(DiffListRow::Header("Config & CI"))
        ));
        assert!(matches!(rows.get(1), Some(DiffListRow::Entry(0))));
        assert!(matches!(rows.get(2), Some(DiffListRow::Header("Source"))));
        assert!(matches!(rows.get(3), Some(DiffListRow::Entry(1))));
    }

    #[test]
    fn auto_layout_prefers_stacked_for_narrow_terminals() {
        assert_eq!(
            DiffTuiLayoutMode::Auto.resolve(Rect::new(0, 0, 80, 24)),
            DiffTuiResolvedLayout::Split
        );
        assert_eq!(
            DiffTuiLayoutMode::Auto.resolve(Rect::new(0, 0, 68, 24)),
            DiffTuiResolvedLayout::Stacked
        );
        assert_eq!(
            DiffTuiLayoutMode::Auto.resolve(Rect::new(0, 0, 56, 20)),
            DiffTuiResolvedLayout::PatchOnly
        );
    }

    #[test]
    fn compact_terminal_uses_shorter_footer_and_selection_panels() {
        let area = Rect::new(0, 0, 80, 24);
        assert_eq!(
            overview_panel_height(false, area, DiffTuiResolvedLayout::Split),
            2
        );
        assert_eq!(
            overview_panel_height(true, area, DiffTuiResolvedLayout::Split),
            3
        );
        assert_eq!(footer_panel_height(area, DiffTuiResolvedLayout::Split), 1);
        assert_eq!(
            selection_panel_height(Rect::new(0, 0, 52, 12), DiffTuiResolvedLayout::Split),
            4
        );
    }

    #[test]
    fn minimal_file_header_keeps_add_delete_columns() {
        let header = line_text(file_header_line(30, 3, DiffTuiRowDensity::Minimal));
        assert!(header.contains("+add"));
        assert!(header.contains("-del"));
        assert!(!header.contains(" sc"));
    }
}
