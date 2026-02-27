//! Persisted za configuration values.

use crate::cli::{ConfigCommands, ConfigKey};
use anyhow::{Context, Result, anyhow, bail};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
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

const CONFIG_ITEMS: [ConfigItem; 5] = [
    ConfigItem {
        key: ConfigKey::GithubToken,
        module: ConfigModule::Auth,
        label: "github-token",
        secret: true,
    },
    ConfigItem {
        key: ConfigKey::RunHttpProxy,
        module: ConfigModule::Run,
        label: "http-proxy",
        secret: false,
    },
    ConfigItem {
        key: ConfigKey::RunHttpsProxy,
        module: ConfigModule::Run,
        label: "https-proxy",
        secret: false,
    },
    ConfigItem {
        key: ConfigKey::RunAllProxy,
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
    Run,
}

#[derive(Default)]
struct ConfigTuiState {
    selected: usize,
    input: Option<String>,
    message: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ZaConfig {
    #[serde(default)]
    auth: AuthConfig,
    #[serde(default)]
    run: RunConfig,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AuthConfig {
    #[serde(default)]
    github_token: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RunConfig {
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
pub struct RunProxyOverrides {
    pub http_proxy: Option<String>,
    pub https_proxy: Option<String>,
    pub all_proxy: Option<String>,
    pub no_proxy: Option<String>,
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
    let Some(path) = maybe_config_path() else {
        return Ok(RunProxyOverrides::default());
    };
    let cfg = read_config(&path)?;
    Ok(RunProxyOverrides {
        http_proxy: cfg.run.http_proxy.and_then(normalize_value),
        https_proxy: cfg.run.https_proxy.and_then(normalize_value),
        all_proxy: cfg.run.all_proxy.and_then(normalize_value),
        no_proxy: cfg.run.no_proxy.and_then(normalize_value),
    })
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
        ConfigKey::RunHttpProxy => cfg.run.http_proxy,
        ConfigKey::RunHttpsProxy => cfg.run.https_proxy,
        ConfigKey::RunAllProxy => cfg.run.all_proxy,
        ConfigKey::RunNoProxy => cfg.run.no_proxy,
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
        ConfigKey::RunHttpProxy => cfg.run.http_proxy = Some(normalized),
        ConfigKey::RunHttpsProxy => cfg.run.https_proxy = Some(normalized),
        ConfigKey::RunAllProxy => cfg.run.all_proxy = Some(normalized),
        ConfigKey::RunNoProxy => cfg.run.no_proxy = Some(normalized),
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
        ConfigKey::RunHttpProxy => cfg.run.http_proxy = None,
        ConfigKey::RunHttpsProxy => cfg.run.https_proxy = None,
        ConfigKey::RunAllProxy => cfg.run.all_proxy = None,
        ConfigKey::RunNoProxy => cfg.run.no_proxy = None,
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
        ConfigKey::RunHttpProxy => "run-http-proxy",
        ConfigKey::RunHttpsProxy => "run-https-proxy",
        ConfigKey::RunAllProxy => "run-all-proxy",
        ConfigKey::RunNoProxy => "run-no-proxy",
    }
}

fn config_item_label(item: &ConfigItem) -> String {
    format!("{}.{}", module_label(item.module), item.label)
}

fn module_label(module: ConfigModule) -> &'static str {
    match module {
        ConfigModule::Auth => "auth",
        ConfigModule::Run => "run",
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
    loop {
        let path = config_path()?;
        let cfg = read_config(&path)?;
        if state.selected >= CONFIG_ITEMS.len() {
            state.selected = CONFIG_ITEMS.len().saturating_sub(1);
        }

        terminal
            .draw(|frame| draw_tui(frame, &cfg, &path, &state))
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
                    let Some(item) = CONFIG_ITEMS.get(state.selected) else {
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
                if state.selected + 1 < CONFIG_ITEMS.len() {
                    state.selected += 1;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                state.selected = state.selected.saturating_sub(1);
            }
            KeyCode::Enter | KeyCode::Char('e') => {
                let Some(item) = CONFIG_ITEMS.get(state.selected) else {
                    continue;
                };
                let current = config_value_by_key(&cfg, item.key)
                    .and_then(|value| normalize_value(value.to_string()))
                    .unwrap_or_default();
                state.input = Some(if item.secret { String::new() } else { current });
                state.message = Some(format!("editing {}", config_item_label(item)));
            }
            KeyCode::Char('u') => {
                let Some(item) = CONFIG_ITEMS.get(state.selected) else {
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

fn draw_tui(frame: &mut ratatui::Frame<'_>, cfg: &ZaConfig, path: &Path, state: &ConfigTuiState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(3),
        ])
        .split(frame.area());

    let title = Paragraph::new(vec![
        Line::from(Span::styled(
            "za config",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::raw(format!("path: {}", path.display()))),
    ])
    .alignment(Alignment::Left)
    .block(Block::default().borders(Borders::ALL).title("Overview"));
    frame.render_widget(title, chunks[0]);

    let body_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(8)])
        .split(chunks[1]);

    render_module_list(
        frame,
        body_chunks[0],
        "auth",
        ConfigModule::Auth,
        cfg,
        state.selected,
    );
    render_module_list(
        frame,
        body_chunks[1],
        "run",
        ConfigModule::Run,
        cfg,
        state.selected,
    );

    let hint = if state.input.is_some() {
        "edit mode: type value, Enter save, Esc cancel, empty value unsets"
    } else {
        "navigate: ↑/↓ or j/k, Enter edit, u unset, q quit"
    };
    let message = state.message.as_deref().unwrap_or(hint);
    let footer =
        Paragraph::new(message).block(Block::default().borders(Borders::ALL).title("Hints"));
    frame.render_widget(footer, chunks[2]);

    if let Some(input) = &state.input {
        let area = centered_rect(70, 24, frame.area());
        frame.render_widget(Clear, area);
        let popup = Block::default().borders(Borders::ALL).title("Edit value");
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
        frame.render_widget(Paragraph::new(input.clone()), inner[1]);
        let x = inner[1].x + input.chars().count() as u16;
        frame.set_cursor_position((x, inner[1].y));
    }
}

fn render_module_list(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    title: &str,
    module: ConfigModule,
    cfg: &ZaConfig,
    selected: usize,
) {
    let indexed_items: Vec<(usize, &ConfigItem)> = CONFIG_ITEMS
        .iter()
        .enumerate()
        .filter(|(_, item)| item.module == module)
        .collect();

    let items: Vec<ListItem<'_>> = indexed_items
        .iter()
        .map(|(_, item)| {
            let value = display_value(config_value_by_key(cfg, item.key), item.secret);
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:<16}", item.label),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(value),
            ]))
        })
        .collect();

    let list = List::new(items)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .block(Block::default().borders(Borders::ALL).title(title));
    let mut list_state = ListState::default();
    let selected_local = indexed_items
        .iter()
        .position(|(global_idx, _)| *global_idx == selected);
    list_state.select(selected_local);
    frame.render_stateful_widget(list, area, &mut list_state);
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
        ConfigKey::RunHttpProxy => cfg.run.http_proxy.as_deref(),
        ConfigKey::RunHttpsProxy => cfg.run.https_proxy.as_deref(),
        ConfigKey::RunAllProxy => cfg.run.all_proxy.as_deref(),
        ConfigKey::RunNoProxy => cfg.run.no_proxy.as_deref(),
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
