//! Persisted za configuration values.

use crate::cli::{ConfigCommands, ConfigKey};
use anyhow::{Context, Result, anyhow, bail};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    style::available_color_count,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
    widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState},
};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    time::Duration,
};

const CONFIG_DIR_NAME: &str = "za";
const CONFIG_FILE_NAME: &str = "config.toml";

const CONFIG_MODULES: [ConfigModule; 5] = [
    ConfigModule::Auth,
    ConfigModule::Proxy,
    ConfigModule::Run,
    ConfigModule::Tool,
    ConfigModule::Update,
];

const CONFIG_ITEMS: [ConfigItem; 17] = [
    ConfigItem {
        key: ConfigKey::GithubToken,
        module: ConfigModule::Auth,
        label: "github-token",
        secret: true,
    },
    ConfigItem {
        key: ConfigKey::ProxyHttp,
        module: ConfigModule::Proxy,
        label: "http-proxy",
        secret: false,
    },
    ConfigItem {
        key: ConfigKey::ProxyHttps,
        module: ConfigModule::Proxy,
        label: "https-proxy",
        secret: false,
    },
    ConfigItem {
        key: ConfigKey::ProxyAll,
        module: ConfigModule::Proxy,
        label: "all-proxy",
        secret: false,
    },
    ConfigItem {
        key: ConfigKey::ProxyNoProxy,
        module: ConfigModule::Proxy,
        label: "no-proxy",
        secret: false,
    },
    ConfigItem {
        key: ConfigKey::RunHttp,
        module: ConfigModule::Run,
        label: "http-proxy",
        secret: false,
    },
    ConfigItem {
        key: ConfigKey::RunHttps,
        module: ConfigModule::Run,
        label: "https-proxy",
        secret: false,
    },
    ConfigItem {
        key: ConfigKey::RunAll,
        module: ConfigModule::Run,
        label: "all-proxy",
        secret: false,
    },
    ConfigItem {
        key: ConfigKey::RunNoProxy,
        module: ConfigModule::Run,
        label: "no-proxy",
        secret: false,
    },
    ConfigItem {
        key: ConfigKey::ToolHttp,
        module: ConfigModule::Tool,
        label: "http-proxy",
        secret: false,
    },
    ConfigItem {
        key: ConfigKey::ToolHttps,
        module: ConfigModule::Tool,
        label: "https-proxy",
        secret: false,
    },
    ConfigItem {
        key: ConfigKey::ToolAll,
        module: ConfigModule::Tool,
        label: "all-proxy",
        secret: false,
    },
    ConfigItem {
        key: ConfigKey::ToolNoProxy,
        module: ConfigModule::Tool,
        label: "no-proxy",
        secret: false,
    },
    ConfigItem {
        key: ConfigKey::UpdateHttp,
        module: ConfigModule::Update,
        label: "http-proxy",
        secret: false,
    },
    ConfigItem {
        key: ConfigKey::UpdateHttps,
        module: ConfigModule::Update,
        label: "https-proxy",
        secret: false,
    },
    ConfigItem {
        key: ConfigKey::UpdateAll,
        module: ConfigModule::Update,
        label: "all-proxy",
        secret: false,
    },
    ConfigItem {
        key: ConfigKey::UpdateNoProxy,
        module: ConfigModule::Update,
        label: "no-proxy",
        secret: false,
    },
];

#[derive(Clone, Copy)]
struct ConfigItem {
    key: ConfigKey,
    module: ConfigModule,
    label: &'static str,
    secret: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ConfigModule {
    Auth,
    Proxy,
    Run,
    Tool,
    Update,
}

#[derive(Default)]
struct ConfigTuiState {
    selected: usize,
    scroll_offset: usize,
    viewport_rows: usize,
    input: Option<String>,
    message: Option<String>,
    show_help: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum TuiDensity {
    Compact,
    Comfortable,
}

#[derive(Clone, Copy)]
struct TuiTheme {
    title: Style,
    module: Style,
    key: Style,
    value: Style,
    selected: Style,
    scrollbar_thumb: Style,
    scrollbar_track: Style,
    highlight_symbol: &'static str,
    scrollbar_thumb_symbol: &'static str,
    scrollbar_track_symbol: Option<&'static str>,
    scrollbar_begin_symbol: Option<&'static str>,
    scrollbar_end_symbol: Option<&'static str>,
}

impl TuiTheme {
    fn detect() -> Self {
        let no_color = env::var_os("NO_COLOR").is_some()
            || matches!(env::var("CLICOLOR").ok().as_deref(), Some("0"))
            || matches!(env::var("TERM").ok().as_deref(), Some("dumb"));
        let supports_color = available_color_count() >= 8;

        if no_color || !supports_color {
            return Self {
                title: Style::default().add_modifier(Modifier::BOLD),
                module: Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                key: Style::default().add_modifier(Modifier::BOLD),
                value: Style::default().add_modifier(Modifier::DIM),
                selected: Style::default().add_modifier(Modifier::REVERSED),
                scrollbar_thumb: Style::default().add_modifier(Modifier::BOLD),
                scrollbar_track: Style::default().add_modifier(Modifier::DIM),
                highlight_symbol: "> ",
                scrollbar_thumb_symbol: "#",
                scrollbar_track_symbol: Some("|"),
                scrollbar_begin_symbol: Some("^"),
                scrollbar_end_symbol: Some("v"),
            };
        }

        Self {
            title: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            module: Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            key: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            value: Style::default().add_modifier(Modifier::DIM),
            selected: Style::default().add_modifier(Modifier::REVERSED),
            scrollbar_thumb: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            scrollbar_track: Style::default().fg(Color::DarkGray),
            highlight_symbol: ">> ",
            scrollbar_thumb_symbol: "#",
            scrollbar_track_symbol: Some("|"),
            scrollbar_begin_symbol: Some("^"),
            scrollbar_end_symbol: Some("v"),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ZaConfig {
    #[serde(default)]
    auth: AuthConfig,
    #[serde(default)]
    proxy: ProxyConfig,
    #[serde(default)]
    run: ProxyConfig,
    #[serde(default)]
    tool: ProxyConfig,
    #[serde(default)]
    update: ProxyConfig,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AuthConfig {
    #[serde(default)]
    github_token: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ProxyConfig {
    #[serde(default)]
    http_proxy: Option<String>,
    #[serde(default)]
    https_proxy: Option<String>,
    #[serde(default)]
    all_proxy: Option<String>,
    #[serde(default)]
    no_proxy: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct ProxyOverrides {
    pub http_proxy: Option<String>,
    pub https_proxy: Option<String>,
    pub all_proxy: Option<String>,
    pub no_proxy: Option<String>,
}

pub type RunProxyOverrides = ProxyOverrides;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProxyScope {
    Run,
    Tool,
    Update,
}

pub fn run(cmd: Option<ConfigCommands>) -> Result<()> {
    match cmd {
        None => run_interactive(),
        Some(ConfigCommands::Path) => {
            let path = config_path()?;
            println!("{}", path.display());
            Ok(())
        }
        Some(ConfigCommands::Set { key, value }) => set_value(key, value),
        Some(ConfigCommands::Get { key, raw }) => get_value(key, raw),
        Some(ConfigCommands::Unset { key }) => unset_value(key),
    }
}

pub fn load_github_token() -> Result<Option<String>> {
    let Some(path) = maybe_config_path() else {
        return Ok(None);
    };
    let cfg = read_config(&path)?;
    Ok(cfg.auth.github_token.and_then(normalize_token))
}

pub fn load_run_proxy_overrides() -> Result<RunProxyOverrides> {
    load_proxy_overrides(ProxyScope::Run)
}

pub fn load_proxy_overrides(scope: ProxyScope) -> Result<ProxyOverrides> {
    let Some(path) = maybe_config_path() else {
        return Ok(ProxyOverrides::default());
    };
    let cfg = read_config(&path)?;
    let global = normalize_proxy_config(&cfg.proxy);
    let scoped = match scope {
        ProxyScope::Run => normalize_proxy_config(&cfg.run),
        ProxyScope::Tool => normalize_proxy_config(&cfg.tool),
        ProxyScope::Update => normalize_proxy_config(&cfg.update),
    };
    Ok(merge_proxy_overrides(&global, &scoped))
}

fn set_value(key: ConfigKey, value: String) -> Result<()> {
    set_value_impl(key, value, true)?;
    Ok(())
}

fn get_value(key: ConfigKey, raw: bool) -> Result<()> {
    let path = config_path()?;
    let cfg = read_config(&path)?;
    let value = match key {
        ConfigKey::GithubToken => cfg.auth.github_token,
        ConfigKey::ProxyHttp => cfg.proxy.http_proxy,
        ConfigKey::ProxyHttps => cfg.proxy.https_proxy,
        ConfigKey::ProxyAll => cfg.proxy.all_proxy,
        ConfigKey::ProxyNoProxy => cfg.proxy.no_proxy,
        ConfigKey::RunHttp => cfg.run.http_proxy,
        ConfigKey::RunHttps => cfg.run.https_proxy,
        ConfigKey::RunAll => cfg.run.all_proxy,
        ConfigKey::RunNoProxy => cfg.run.no_proxy,
        ConfigKey::ToolHttp => cfg.tool.http_proxy,
        ConfigKey::ToolHttps => cfg.tool.https_proxy,
        ConfigKey::ToolAll => cfg.tool.all_proxy,
        ConfigKey::ToolNoProxy => cfg.tool.no_proxy,
        ConfigKey::UpdateHttp => cfg.update.http_proxy,
        ConfigKey::UpdateHttps => cfg.update.https_proxy,
        ConfigKey::UpdateAll => cfg.update.all_proxy,
        ConfigKey::UpdateNoProxy => cfg.update.no_proxy,
    }
    .and_then(normalize_value);

    match value {
        Some(value) if raw => println!("{value}"),
        Some(value) if key == ConfigKey::GithubToken => println!("{}", mask_secret(&value)),
        Some(value) => println!("{value}"),
        None => println!("<unset>"),
    }
    Ok(())
}

fn unset_value(key: ConfigKey) -> Result<()> {
    unset_value_impl(key, true)?;
    Ok(())
}

fn set_value_impl(key: ConfigKey, value: String, print_result: bool) -> Result<()> {
    let path = config_path()?;
    let normalized = normalize_value(value).ok_or_else(|| anyhow!("value cannot be empty"))?;
    let mut cfg = read_config(&path)?;
    match key {
        ConfigKey::GithubToken => cfg.auth.github_token = Some(normalized),
        ConfigKey::ProxyHttp => cfg.proxy.http_proxy = Some(normalized),
        ConfigKey::ProxyHttps => cfg.proxy.https_proxy = Some(normalized),
        ConfigKey::ProxyAll => cfg.proxy.all_proxy = Some(normalized),
        ConfigKey::ProxyNoProxy => cfg.proxy.no_proxy = Some(normalized),
        ConfigKey::RunHttp => cfg.run.http_proxy = Some(normalized),
        ConfigKey::RunHttps => cfg.run.https_proxy = Some(normalized),
        ConfigKey::RunAll => cfg.run.all_proxy = Some(normalized),
        ConfigKey::RunNoProxy => cfg.run.no_proxy = Some(normalized),
        ConfigKey::ToolHttp => cfg.tool.http_proxy = Some(normalized),
        ConfigKey::ToolHttps => cfg.tool.https_proxy = Some(normalized),
        ConfigKey::ToolAll => cfg.tool.all_proxy = Some(normalized),
        ConfigKey::ToolNoProxy => cfg.tool.no_proxy = Some(normalized),
        ConfigKey::UpdateHttp => cfg.update.http_proxy = Some(normalized),
        ConfigKey::UpdateHttps => cfg.update.https_proxy = Some(normalized),
        ConfigKey::UpdateAll => cfg.update.all_proxy = Some(normalized),
        ConfigKey::UpdateNoProxy => cfg.update.no_proxy = Some(normalized),
    }
    write_config(&path, &cfg)?;
    if print_result {
        println!("updated {} in {}", key_label(key), path.display());
    }
    Ok(())
}

fn unset_value_impl(key: ConfigKey, print_result: bool) -> Result<()> {
    let path = config_path()?;
    let mut cfg = read_config(&path)?;
    match key {
        ConfigKey::GithubToken => cfg.auth.github_token = None,
        ConfigKey::ProxyHttp => cfg.proxy.http_proxy = None,
        ConfigKey::ProxyHttps => cfg.proxy.https_proxy = None,
        ConfigKey::ProxyAll => cfg.proxy.all_proxy = None,
        ConfigKey::ProxyNoProxy => cfg.proxy.no_proxy = None,
        ConfigKey::RunHttp => cfg.run.http_proxy = None,
        ConfigKey::RunHttps => cfg.run.https_proxy = None,
        ConfigKey::RunAll => cfg.run.all_proxy = None,
        ConfigKey::RunNoProxy => cfg.run.no_proxy = None,
        ConfigKey::ToolHttp => cfg.tool.http_proxy = None,
        ConfigKey::ToolHttps => cfg.tool.https_proxy = None,
        ConfigKey::ToolAll => cfg.tool.all_proxy = None,
        ConfigKey::ToolNoProxy => cfg.tool.no_proxy = None,
        ConfigKey::UpdateHttp => cfg.update.http_proxy = None,
        ConfigKey::UpdateHttps => cfg.update.https_proxy = None,
        ConfigKey::UpdateAll => cfg.update.all_proxy = None,
        ConfigKey::UpdateNoProxy => cfg.update.no_proxy = None,
    }
    write_config(&path, &cfg)?;
    if print_result {
        println!("removed {} from {}", key_label(key), path.display());
    }
    Ok(())
}

fn key_label(key: ConfigKey) -> &'static str {
    match key {
        ConfigKey::GithubToken => "github-token",
        ConfigKey::ProxyHttp => "proxy-http",
        ConfigKey::ProxyHttps => "proxy-https",
        ConfigKey::ProxyAll => "proxy-all",
        ConfigKey::ProxyNoProxy => "proxy-no-proxy",
        ConfigKey::RunHttp => "run-http",
        ConfigKey::RunHttps => "run-https",
        ConfigKey::RunAll => "run-all",
        ConfigKey::RunNoProxy => "run-no-proxy",
        ConfigKey::ToolHttp => "tool-http",
        ConfigKey::ToolHttps => "tool-https",
        ConfigKey::ToolAll => "tool-all",
        ConfigKey::ToolNoProxy => "tool-no-proxy",
        ConfigKey::UpdateHttp => "update-http",
        ConfigKey::UpdateHttps => "update-https",
        ConfigKey::UpdateAll => "update-all",
        ConfigKey::UpdateNoProxy => "update-no-proxy",
    }
}

fn config_item_label(item: &ConfigItem) -> String {
    format!("{}.{}", module_label(item.module), item.label)
}

fn module_label(module: ConfigModule) -> &'static str {
    match module {
        ConfigModule::Auth => "auth",
        ConfigModule::Proxy => "proxy",
        ConfigModule::Run => "run",
        ConfigModule::Tool => "tool",
        ConfigModule::Update => "update",
    }
}

fn run_interactive() -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!(
            "`za config` interactive mode requires a TTY; use `za config set/get/unset/path` in non-interactive environments"
        );
    }

    enable_raw_mode().context("enable raw terminal mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create ratatui terminal")?;

    let result = run_tui_loop(&mut terminal);

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
    Ok(())
}

fn display_value(value: Option<&str>, secret: bool) -> String {
    let Some(value) = value.and_then(|v| normalize_value(v.to_string())) else {
        return "<unset>".to_string();
    };
    if secret { mask_secret(&value) } else { value }
}

fn run_tui_loop(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
) -> Result<()> {
    let mut state = ConfigTuiState::default();
    let theme = TuiTheme::detect();
    loop {
        let path = config_path()?;
        let cfg = read_config(&path)?;
        let display_order = display_order_indices();
        if state.selected >= display_order.len() {
            state.selected = display_order.len().saturating_sub(1);
        }

        terminal
            .draw(|frame| draw_tui(frame, &cfg, &path, &mut state, &display_order, &theme))
            .context("draw config tui")?;

        if !event::poll(Duration::from_millis(120)).context("poll keyboard events")? {
            continue;
        }
        let Event::Key(key) = event::read().context("read keyboard event")? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        if state.input.is_some() {
            match key.code {
                KeyCode::Esc => {
                    state.input = None;
                    state.message = Some("edit canceled".to_string());
                }
                KeyCode::Enter => {
                    let Some(selected_idx) = display_order.get(state.selected).copied() else {
                        state.input = None;
                        continue;
                    };
                    let Some(item) = CONFIG_ITEMS.get(selected_idx) else {
                        state.input = None;
                        continue;
                    };
                    let input = state.input.take().unwrap_or_default();
                    if input.trim().is_empty() {
                        match unset_value_impl(item.key, false) {
                            Ok(()) => {
                                state.message = Some(format!("unset {}", config_item_label(item)))
                            }
                            Err(err) => state.message = Some(format!("error: {err}")),
                        }
                    } else {
                        match set_value_impl(item.key, input, false) {
                            Ok(()) => {
                                state.message = Some(format!("updated {}", config_item_label(item)))
                            }
                            Err(err) => state.message = Some(format!("error: {err}")),
                        }
                    }
                }
                KeyCode::Backspace => {
                    if let Some(input) = state.input.as_mut() {
                        input.pop();
                    }
                }
                KeyCode::Char(ch) => {
                    if let Some(input) = state.input.as_mut() {
                        input.push(ch);
                    }
                }
                _ => {}
            }
            continue;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Down | KeyCode::Char('j') => {
                if state.selected + 1 < display_order.len() {
                    state.selected += 1;
                    state.message = None;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let before = state.selected;
                state.selected = state.selected.saturating_sub(1);
                if state.selected != before {
                    state.message = None;
                }
            }
            KeyCode::Home => {
                if !display_order.is_empty() {
                    state.selected = 0;
                    state.message = None;
                }
            }
            KeyCode::End => {
                if !display_order.is_empty() {
                    state.selected = display_order.len().saturating_sub(1);
                    state.message = None;
                }
            }
            KeyCode::PageDown => {
                if !display_order.is_empty() {
                    let step = state.viewport_rows.saturating_sub(1).max(1);
                    let max = display_order.len().saturating_sub(1);
                    state.selected = state.selected.saturating_add(step).min(max);
                    state.message = None;
                }
            }
            KeyCode::PageUp => {
                if !display_order.is_empty() {
                    let step = state.viewport_rows.saturating_sub(1).max(1);
                    state.selected = state.selected.saturating_sub(step);
                    state.message = None;
                }
            }
            KeyCode::Char('?') | KeyCode::F(1) => {
                state.show_help = !state.show_help;
                state.message = Some(if state.show_help {
                    "help panel shown".to_string()
                } else {
                    "help panel hidden".to_string()
                });
            }
            KeyCode::Enter | KeyCode::Char('e') => {
                let Some(selected_idx) = display_order.get(state.selected).copied() else {
                    continue;
                };
                let Some(item) = CONFIG_ITEMS.get(selected_idx) else {
                    continue;
                };
                let current = config_value_by_key(&cfg, item.key)
                    .and_then(|value| normalize_value(value.to_string()))
                    .unwrap_or_default();
                state.input = Some(if item.secret { String::new() } else { current });
                state.message = Some(format!("editing {}", config_item_label(item)));
            }
            KeyCode::Char('u') => {
                let Some(selected_idx) = display_order.get(state.selected).copied() else {
                    continue;
                };
                let Some(item) = CONFIG_ITEMS.get(selected_idx) else {
                    continue;
                };
                match unset_value_impl(item.key, false) {
                    Ok(()) => state.message = Some(format!("unset {}", config_item_label(item))),
                    Err(err) => state.message = Some(format!("error: {err}")),
                }
            }
            _ => {}
        }
    }
}

fn draw_tui(
    frame: &mut ratatui::Frame<'_>,
    cfg: &ZaConfig,
    path: &Path,
    state: &mut ConfigTuiState,
    display_order: &[usize],
    theme: &TuiTheme,
) {
    let density = resolve_tui_density(frame.area());
    let status_height = 4;
    let help_height = if state.show_help {
        match density {
            TuiDensity::Compact => 5,
            TuiDensity::Comfortable => 7,
        }
    } else {
        0
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(6),
            Constraint::Length(status_height + help_height),
        ])
        .split(frame.area());

    let title_width = usize::from(chunks[0].width.saturating_sub(2)).max(1);
    let path_raw = path.display().to_string();
    let mode_label = match density {
        TuiDensity::Compact => "compact",
        TuiDensity::Comfortable => "comfortable",
    };
    let title_sub = match density {
        TuiDensity::Compact => format!(
            "mode: {mode_label} | help: {}",
            if state.show_help { "on" } else { "off" }
        ),
        TuiDensity::Comfortable => format!("path: {path_raw}"),
    };
    let title = Paragraph::new(vec![
        Line::from(Span::styled("za config", theme.title)),
        Line::from(Span::styled(
            truncate_middle(&title_sub, title_width),
            theme.value,
        )),
    ])
    .alignment(Alignment::Left)
    .block(Block::default().borders(Borders::ALL).title("Overview"));
    frame.render_widget(title, chunks[0]);

    let selected_item_index = display_order.get(state.selected).copied();
    let block = Block::default().borders(Borders::ALL).title("Config");
    let inner = block.inner(chunks[1]);
    frame.render_widget(block, chunks[1]);

    let (rows, row_item_indices) = build_config_rows(cfg, density, *theme, inner.width);
    let selected_row = row_item_indices
        .iter()
        .position(|idx| *idx == selected_item_index)
        .unwrap_or_default();

    let mut list_state = ListState::default()
        .with_offset(state.scroll_offset)
        .with_selected(Some(selected_row));

    let list = List::new(rows)
        .highlight_style(theme.selected)
        .highlight_symbol(theme.highlight_symbol);
    frame.render_stateful_widget(list, inner, &mut list_state);

    let row_count = row_item_indices.len();
    let viewport_len = usize::from(inner.height.max(1));
    state.viewport_rows = viewport_len;
    state.scroll_offset = list_state.offset().min(row_count.saturating_sub(1));

    let mut scrollbar_state = ScrollbarState::new(row_count)
        .position(list_state.offset())
        .viewport_content_length(viewport_len);
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .thumb_symbol(theme.scrollbar_thumb_symbol)
        .track_symbol(theme.scrollbar_track_symbol)
        .begin_symbol(theme.scrollbar_begin_symbol)
        .end_symbol(theme.scrollbar_end_symbol)
        .thumb_style(theme.scrollbar_thumb)
        .track_style(theme.scrollbar_track)
        .begin_style(theme.scrollbar_track)
        .end_style(theme.scrollbar_track);
    frame.render_stateful_widget(scrollbar, inner, &mut scrollbar_state);

    let visible_start = state.scroll_offset.saturating_add(1).min(row_count.max(1));
    let visible_end = state
        .scroll_offset
        .saturating_add(viewport_len)
        .min(row_count.max(1));
    let selected_label = selected_item_index
        .and_then(|idx| CONFIG_ITEMS.get(idx))
        .map(config_item_label)
        .unwrap_or_else(|| "<none>".to_string());
    let selected_info = format!(
        "selected: {} ({}/{})",
        selected_label,
        state.selected.saturating_add(1),
        display_order.len()
    );
    let scroll_info = format!(
        "view: rows {}-{} / {}",
        visible_start, visible_end, row_count
    );
    let hint = if state.input.is_some() {
        "edit: Enter save | Esc cancel | empty value unsets"
    } else {
        "normal: Enter/e edit | u unset | ?/F1 help | q quit"
    };
    let primary_status = state.message.as_deref().unwrap_or(hint);
    let secondary_status = format!("{selected_info} | {scroll_info}");

    if state.show_help {
        let footer_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(status_height),
                Constraint::Min(help_height),
            ])
            .split(chunks[2]);
        let status_width = usize::from(footer_chunks[0].width.saturating_sub(2)).max(1);
        let status = Paragraph::new(vec![
            Line::from(Span::raw(truncate_end(primary_status, status_width))),
            Line::from(Span::styled(
                truncate_end(&secondary_status, status_width),
                theme.value,
            )),
        ])
        .block(Block::default().borders(Borders::ALL).title("Status"));
        frame.render_widget(status, footer_chunks[0]);

        let help_width = usize::from(footer_chunks[1].width.saturating_sub(2)).max(1);
        let help_lines = help_text_lines(density)
            .into_iter()
            .map(|line| Line::from(Span::raw(truncate_end(line, help_width))))
            .collect::<Vec<_>>();
        let help =
            Paragraph::new(help_lines).block(Block::default().borders(Borders::ALL).title("Help"));
        frame.render_widget(help, footer_chunks[1]);
    } else {
        let status_width = usize::from(chunks[2].width.saturating_sub(2)).max(1);
        let status = Paragraph::new(vec![
            Line::from(Span::raw(truncate_end(primary_status, status_width))),
            Line::from(Span::styled(
                truncate_end(&secondary_status, status_width),
                theme.value,
            )),
        ])
        .block(Block::default().borders(Borders::ALL).title("Status"));
        frame.render_widget(status, chunks[2]);
    }

    if let Some(input) = &state.input {
        let area = centered_rect(70, 24, frame.area());
        frame.render_widget(Clear, area);
        let popup = Block::default().borders(Borders::ALL).title("Edit Value");
        frame.render_widget(popup, area);

        let inner = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(1),
            ])
            .margin(1)
            .split(area);
        frame.render_widget(
            Paragraph::new("Enter value, leave empty to unset"),
            inner[0],
        );
        frame.render_widget(Paragraph::new(input.clone()).style(theme.value), inner[1]);
        let x = inner[1].x + input.chars().count() as u16;
        frame.set_cursor_position((x, inner[1].y));
    }
}

fn build_config_rows(
    cfg: &ZaConfig,
    density: TuiDensity,
    theme: TuiTheme,
    content_width: u16,
) -> (Vec<ListItem<'static>>, Vec<Option<usize>>) {
    let mut rows = Vec::new();
    let mut row_item_indices = Vec::new();
    let compact = density == TuiDensity::Compact;
    let width = usize::from(content_width.max(1));
    let key_width = if compact {
        width.saturating_mul(2) / 5
    } else {
        14
    }
    .clamp(10, 28);
    let value_width = width.saturating_sub(key_width + 4).max(8);

    let mut active_module: Option<ConfigModule> = None;
    for idx in display_order_indices() {
        let item = CONFIG_ITEMS[idx];
        if !compact && active_module != Some(item.module) {
            active_module = Some(item.module);
            let header = format!("[{}]", module_label(item.module));
            rows.push(ListItem::new(Line::from(Span::styled(
                header,
                theme.module,
            ))));
            row_item_indices.push(None);
        }

        let key_source = if compact {
            format!("{}.{}", module_label(item.module), item.label)
        } else {
            item.label.to_string()
        };
        let key_cell = pad_to_width(&truncate_end(&key_source, key_width), key_width);
        let value_raw = display_value(config_value_by_key(cfg, item.key), item.secret);
        let value_cell = truncate_middle(&value_raw, value_width);
        rows.push(ListItem::new(Line::from(vec![
            Span::raw(" "),
            Span::styled(key_cell, theme.key),
            Span::raw(" "),
            Span::styled(value_cell, theme.value),
        ])));
        row_item_indices.push(Some(idx));
    }

    (rows, row_item_indices)
}

fn resolve_tui_density(area: Rect) -> TuiDensity {
    if area.width < 100 || area.height < 26 {
        TuiDensity::Compact
    } else {
        TuiDensity::Comfortable
    }
}

fn help_text_lines(density: TuiDensity) -> Vec<&'static str> {
    match density {
        TuiDensity::Compact => vec![
            "Move: Up/Down, j/k, PgUp/PgDn, Home/End",
            "Edit: Enter/e | Unset: u | Save: Enter | Cancel: Esc",
            "Help: ? or F1 | Quit: q",
        ],
        TuiDensity::Comfortable => vec![
            "Navigation: Up/Down, j/k, PgUp/PgDn, Home/End",
            "Edit selected item: Enter or e",
            "Unset selected item: u",
            "Save edit: Enter (empty value unsets)",
            "Cancel edit: Esc",
            "Toggle this panel: ? or F1",
            "Quit: q",
        ],
    }
}

fn truncate_end(input: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }
    let head: String = input.chars().take(max_chars - 3).collect();
    format!("{head}...")
}

fn truncate_middle(input: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }
    let head_len = (max_chars - 3) / 2;
    let tail_len = max_chars - 3 - head_len;
    let head: String = input.chars().take(head_len).collect();
    let tail: String = input
        .chars()
        .rev()
        .take(tail_len)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}...{tail}")
}

fn pad_to_width(input: &str, width: usize) -> String {
    let len = input.chars().count();
    if len >= width {
        return input.to_string();
    }
    let mut out = String::with_capacity(width);
    out.push_str(input);
    out.push_str(&" ".repeat(width - len));
    out
}

fn display_order_indices() -> Vec<usize> {
    let mut out = Vec::with_capacity(CONFIG_ITEMS.len());
    for module in CONFIG_MODULES {
        for (idx, item) in CONFIG_ITEMS.iter().enumerate() {
            if item.module == module {
                out.push(idx);
            }
        }
    }
    out
}

fn centered_rect(width_percent: u16, height_percent: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_percent) / 2),
            Constraint::Percentage(height_percent),
            Constraint::Percentage((100 - height_percent) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Percentage((100 - width_percent) / 2),
        ])
        .split(vertical[1])[1]
}

fn config_value_by_key(cfg: &ZaConfig, key: ConfigKey) -> Option<&str> {
    match key {
        ConfigKey::GithubToken => cfg.auth.github_token.as_deref(),
        ConfigKey::ProxyHttp => cfg.proxy.http_proxy.as_deref(),
        ConfigKey::ProxyHttps => cfg.proxy.https_proxy.as_deref(),
        ConfigKey::ProxyAll => cfg.proxy.all_proxy.as_deref(),
        ConfigKey::ProxyNoProxy => cfg.proxy.no_proxy.as_deref(),
        ConfigKey::RunHttp => cfg.run.http_proxy.as_deref(),
        ConfigKey::RunHttps => cfg.run.https_proxy.as_deref(),
        ConfigKey::RunAll => cfg.run.all_proxy.as_deref(),
        ConfigKey::RunNoProxy => cfg.run.no_proxy.as_deref(),
        ConfigKey::ToolHttp => cfg.tool.http_proxy.as_deref(),
        ConfigKey::ToolHttps => cfg.tool.https_proxy.as_deref(),
        ConfigKey::ToolAll => cfg.tool.all_proxy.as_deref(),
        ConfigKey::ToolNoProxy => cfg.tool.no_proxy.as_deref(),
        ConfigKey::UpdateHttp => cfg.update.http_proxy.as_deref(),
        ConfigKey::UpdateHttps => cfg.update.https_proxy.as_deref(),
        ConfigKey::UpdateAll => cfg.update.all_proxy.as_deref(),
        ConfigKey::UpdateNoProxy => cfg.update.no_proxy.as_deref(),
    }
}

fn maybe_config_path() -> Option<PathBuf> {
    Some(config_base_dir()?.join(CONFIG_FILE_NAME))
}

fn config_base_dir() -> Option<PathBuf> {
    if let Some(base) = env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(base).join(CONFIG_DIR_NAME));
    }
    let home = env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config").join(CONFIG_DIR_NAME))
}

fn config_path() -> Result<PathBuf> {
    maybe_config_path().ok_or_else(|| {
        anyhow!("cannot resolve config path: set HOME or XDG_CONFIG_HOME in environment")
    })
}

fn read_config(path: &Path) -> Result<ZaConfig> {
    if !path.exists() {
        return Ok(ZaConfig::default());
    }
    if !path.is_file() {
        bail!("config path is not a file: {}", path.display());
    }

    let raw =
        fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
    toml::from_str::<ZaConfig>(&raw)
        .with_context(|| format!("parse TOML config {}", path.display()))
}

fn write_config(path: &Path, cfg: &ZaConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create config directory {}", parent.display()))?;
    }

    let tmp = path.with_extension("tmp");
    let data = toml::to_string_pretty(cfg).context("serialize TOML config")?;
    fs::write(&tmp, data).with_context(|| format!("write temp config {}", tmp.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        fs::set_permissions(&tmp, perms)
            .with_context(|| format!("set permissions for {}", tmp.display()))?;
    }

    fs::rename(&tmp, path)
        .with_context(|| format!("replace config {} with {}", path.display(), tmp.display()))?;
    Ok(())
}

fn normalize_proxy_config(cfg: &ProxyConfig) -> ProxyOverrides {
    ProxyOverrides {
        http_proxy: cfg.http_proxy.clone().and_then(normalize_value),
        https_proxy: cfg.https_proxy.clone().and_then(normalize_value),
        all_proxy: cfg.all_proxy.clone().and_then(normalize_value),
        no_proxy: cfg.no_proxy.clone().and_then(normalize_value),
    }
}

fn merge_proxy_overrides(global: &ProxyOverrides, scoped: &ProxyOverrides) -> ProxyOverrides {
    ProxyOverrides {
        http_proxy: scoped
            .http_proxy
            .clone()
            .or_else(|| global.http_proxy.clone()),
        https_proxy: scoped
            .https_proxy
            .clone()
            .or_else(|| global.https_proxy.clone()),
        all_proxy: scoped
            .all_proxy
            .clone()
            .or_else(|| global.all_proxy.clone()),
        no_proxy: scoped.no_proxy.clone().or_else(|| global.no_proxy.clone()),
    }
}

fn normalize_token(raw: String) -> Option<String> {
    normalize_value(raw)
}

fn normalize_value(raw: String) -> Option<String> {
    let token = raw.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

fn mask_secret(secret: &str) -> String {
    let chars = secret.chars().count();
    if chars <= 8 {
        return "********".to_string();
    }

    let head: String = secret.chars().take(4).collect();
    let tail: String = secret
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}…{tail}")
}

#[cfg(test)]
mod tests {
    use super::{ProxyOverrides, merge_proxy_overrides};

    #[test]
    fn scoped_proxy_overrides_global_values() {
        let global = ProxyOverrides {
            http_proxy: Some("http://global-http".to_string()),
            https_proxy: Some("http://global-https".to_string()),
            all_proxy: None,
            no_proxy: Some("localhost".to_string()),
        };
        let scoped = ProxyOverrides {
            http_proxy: None,
            https_proxy: Some("http://scoped-https".to_string()),
            all_proxy: Some("socks5://scoped-all".to_string()),
            no_proxy: None,
        };

        let merged = merge_proxy_overrides(&global, &scoped);
        assert_eq!(merged.http_proxy.as_deref(), Some("http://global-http"));
        assert_eq!(merged.https_proxy.as_deref(), Some("http://scoped-https"));
        assert_eq!(merged.all_proxy.as_deref(), Some("socks5://scoped-all"));
        assert_eq!(merged.no_proxy.as_deref(), Some("localhost"));
    }
}
