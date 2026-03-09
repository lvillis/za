use super::*;

pub(super) fn run_top(all: bool, history: bool) -> Result<i32> {
    if !is_interactive_terminal() {
        bail!("`za codex top` requires a TTY");
    }

    let current_workspace_root = resolve_workspace_context()?.workspace_root;
    let mut listener = TopListenerHandle::start()?;
    let mut app = CodexTopApp::new(current_workspace_root, all, history);

    enable_raw_mode().context("enable raw terminal mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create ratatui terminal")?;

    let result = run_top_tui_loop(&mut terminal, &mut app, &mut listener);

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
    if let Err(err) = listener.shutdown() {
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

fn run_top_tui_loop(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    app: &mut CodexTopApp,
    listener: &mut TopListenerHandle,
) -> Result<()> {
    loop {
        app.refresh(listener)?;
        terminal
            .draw(|frame| draw_top_tui(frame, app, listener))
            .context("draw codex top tui")?;

        if !event::poll(Duration::from_millis(120)).context("poll keyboard events")? {
            continue;
        }
        let Event::Key(key) = event::read().context("read keyboard event")? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        match key.code {
            KeyCode::Char('q') => return Ok(()),
            _ => app.handle_key(key.code),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct TopListenerState {
    pub(super) endpoint: String,
    pub(super) owner_pid: u32,
    pub(super) updated_at_unix: u64,
}

#[derive(Clone, Debug, Default)]
pub(super) struct OtelSessionState {
    pub(super) model: Option<String>,
    pub(super) effort: Option<String>,
    pub(super) workspace_root: Option<String>,
    pub(super) last_activity_unix: Option<u64>,
    pub(super) last_event_name: Option<String>,
    pub(super) otel_events: u64,
    pub(super) api_requests: u64,
    pub(super) tool_calls: u64,
    pub(super) tool_errors: u64,
    pub(super) sse_events: u64,
}

#[derive(Clone, Debug, Default)]
pub(super) struct OtelLiveState {
    pub(super) sessions: BTreeMap<String, OtelSessionState>,
    pub(super) session_events: BTreeMap<String, VecDeque<OtelEventRecord>>,
    pub(super) total_events: u64,
    pub(super) last_event_unix: Option<u64>,
}

#[derive(Clone, Debug)]
pub(super) struct OtelEventRecord {
    pub(super) observed_unix: u64,
    pub(super) event_name: String,
    pub(super) tool_error: bool,
    pub(super) attributes: BTreeMap<String, String>,
    pub(super) body: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) struct OtelSessionEvent {
    pub(super) session_id: String,
    pub(super) event_name: String,
    pub(super) observed_unix: u64,
    pub(super) model: Option<String>,
    pub(super) effort: Option<String>,
    pub(super) workspace_root: Option<String>,
    pub(super) tool_error: bool,
    pub(super) attributes: BTreeMap<String, String>,
    pub(super) body: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OtlpLogsPayload {
    #[serde(rename = "resourceLogs", default)]
    resource_logs: Vec<OtlpResourceLogs>,
}

#[derive(Debug, Deserialize)]
struct OtlpResourceLogs {
    #[serde(rename = "scopeLogs", default)]
    scope_logs: Vec<OtlpScopeLogs>,
}

#[derive(Debug, Deserialize)]
struct OtlpScopeLogs {
    #[serde(rename = "logRecords", default)]
    log_records: Vec<OtlpLogRecord>,
}

#[derive(Debug, Deserialize)]
struct OtlpLogRecord {
    #[serde(rename = "observedTimeUnixNano")]
    observed_time_unix_nano: Option<String>,
    body: Option<serde_json::Value>,
    #[serde(default)]
    attributes: Vec<OtlpAttribute>,
}

#[derive(Debug, Deserialize)]
struct OtlpAttribute {
    key: String,
    value: serde_json::Value,
}

#[derive(Clone, Debug)]
pub(super) struct CodexTopRow {
    pub(super) key: String,
    pub(super) session_id: Option<String>,
    pub(super) managed_session_name: Option<String>,
    pub(super) workspace_root: String,
    pub(super) model: Option<String>,
    pub(super) effort: Option<String>,
    pub(super) context_left_percent: Option<f64>,
    pub(super) status: String,
    pub(super) tmux_running: bool,
    pub(super) attached_clients: usize,
    pub(super) last_activity_unix: Option<u64>,
    pub(super) last_event_name: Option<String>,
    pub(super) otel_events: u64,
    pub(super) api_requests: u64,
    pub(super) live_tool_calls: u64,
    pub(super) lifetime_tool_calls: u64,
    pub(super) live_tool_errors: u64,
    pub(super) lifetime_tool_errors: u64,
    pub(super) sse_events: u64,
    pub(super) live_otel: bool,
}

#[derive(Debug)]
pub(super) struct CodexTopApp {
    pub(super) current_workspace_root: String,
    pub(super) show_all: bool,
    pub(super) show_history: bool,
    pub(super) selected: usize,
    pub(super) scroll_offset: usize,
    pub(super) viewport_rows: usize,
    pub(super) rows: Vec<CodexTopRow>,
    pub(super) trackers: BTreeMap<PathBuf, SessionFileTracker>,
    pub(super) otel_state: OtelLiveState,
    pub(super) tmux_available: bool,
    pub(super) tmux_sessions: BTreeMap<String, TmuxSessionInfo>,
    pub(super) managed_records: Vec<SessionRecord>,
    pub(super) last_refresh: Option<SystemTime>,
    pub(super) last_discovery: Option<SystemTime>,
    pub(super) status_message: Option<String>,
    pub(super) view: TopView,
}

#[derive(Debug)]
pub(super) enum TopView {
    Summary,
    Stream(TopStreamState),
}

#[derive(Debug)]
pub(super) struct TopStreamState {
    pub(super) session_id: String,
    pub(super) workspace_root: String,
    pub(super) model: Option<String>,
    pub(super) effort: Option<String>,
    pub(super) tmux_running: bool,
    pub(super) live_otel: bool,
    pub(super) selected: usize,
    pub(super) scroll_offset: usize,
    pub(super) viewport_rows: usize,
    pub(super) follow: bool,
}

struct TopListenerHandle {
    endpoint: String,
    receiver: Receiver<OtelSessionEvent>,
    state_path: PathBuf,
    state: TopListenerState,
    stop: Arc<AtomicBool>,
    join_handle: Option<thread::JoinHandle<()>>,
    last_heartbeat: SystemTime,
}

pub(super) struct TopRowsInput<'a> {
    pub(super) current_workspace_root: &'a str,
    pub(super) show_all: bool,
    pub(super) show_history: bool,
    pub(super) trackers: &'a BTreeMap<PathBuf, SessionFileTracker>,
    pub(super) otel_state: &'a OtelLiveState,
    pub(super) managed_records: &'a [SessionRecord],
    pub(super) tmux_available: bool,
    pub(super) tmux_sessions: &'a BTreeMap<String, TmuxSessionInfo>,
}

pub(super) fn top_listener_state_for_launch(
    extra_args: &[String],
) -> Result<Option<TopListenerState>> {
    if user_supplied_otel_config(extra_args) {
        return Ok(None);
    }

    load_active_top_listener_state()
}

pub(super) fn top_listener_codex_args(listener: Option<&TopListenerState>) -> Vec<String> {
    let Some(listener) = listener else {
        return Vec::new();
    };
    vec![
        "-c".to_string(),
        format!(
            "otel.exporter={{otlp-http={{endpoint=\"{}\",protocol=\"json\"}}}}",
            listener.endpoint
        ),
        "-c".to_string(),
        "otel.log_user_prompt=false".to_string(),
    ]
}

pub(super) fn ensure_local_listener_no_proxy(env_vars: &mut Vec<(String, String)>) {
    const LOCAL_RULES: [&str; 3] = ["127.0.0.1", "localhost", "::1"];

    let mut rules = env_vars
        .iter()
        .find_map(|(key, value)| {
            (key == "NO_PROXY" || key == "no_proxy").then_some(parse_no_proxy_rules(value))
        })
        .unwrap_or_default();
    for rule in LOCAL_RULES {
        if !rules.iter().any(|existing| existing == rule) {
            rules.push(rule.to_string());
        }
    }
    let value = rules.join(",");
    let mut saw_upper = false;
    let mut saw_lower = false;
    for (key, current) in env_vars.iter_mut() {
        if key == "NO_PROXY" {
            *current = value.clone();
            saw_upper = true;
        } else if key == "no_proxy" {
            *current = value.clone();
            saw_lower = true;
        }
    }
    if !saw_upper {
        env_vars.push(("NO_PROXY".to_string(), value.clone()));
    }
    if !saw_lower {
        env_vars.push(("no_proxy".to_string(), value));
    }
}

fn parse_no_proxy_rules(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|rule| !rule.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn user_supplied_otel_config(args: &[String]) -> bool {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "-c" || arg == "--config" {
            if let Some(value) = args.get(index + 1)
                && config_overrides_otel(value)
            {
                return true;
            }
            index += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--config=")
            && config_overrides_otel(value)
        {
            return true;
        }
        index += 1;
    }
    false
}

pub(super) fn config_overrides_otel(value: &str) -> bool {
    value
        .split_once('=')
        .map(|(key, _)| key.trim())
        .is_some_and(|key| key == "otel" || key.starts_with("otel."))
}

fn top_listener_state_path() -> Result<PathBuf> {
    Ok(state_home()?.join(TOP_LISTENER_STATE_RELATIVE))
}

fn load_active_top_listener_state() -> Result<Option<TopListenerState>> {
    let path = top_listener_state_path()?;
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };

    let state = match serde_json::from_slice::<TopListenerState>(&bytes) {
        Ok(state) => state,
        Err(_) => {
            let _ = fs::remove_file(&path);
            return Ok(None);
        }
    };
    let now = current_unix_seconds();
    if now.saturating_sub(state.updated_at_unix) > TOP_LISTENER_STALE_SECS {
        let _ = fs::remove_file(&path);
        return Ok(None);
    }
    Ok(Some(state))
}

fn write_top_listener_state(path: &Path, state: &TopListenerState) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create listener state directory {}", parent.display()))?;
    }
    fs::write(
        path,
        serde_json::to_vec_pretty(state).context("serialize codex top listener state")?,
    )
    .with_context(|| format!("write {}", path.display()))
}

fn remove_top_listener_state(path: &Path, endpoint: &str) -> Result<()> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };
    let should_remove = serde_json::from_slice::<TopListenerState>(&bytes)
        .ok()
        .is_none_or(|state| state.endpoint == endpoint);
    if should_remove {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => return Err(err).with_context(|| format!("remove {}", path.display())),
        }
    }
    Ok(())
}

impl TopListenerHandle {
    fn start() -> Result<Self> {
        if let Some(active) = load_active_top_listener_state()? {
            bail!(
                "another `za codex top` is already running at {}; stop it or wait for its listener state to expire",
                active.endpoint
            );
        }

        let listener =
            TcpListener::bind(("127.0.0.1", 0)).context("bind local Codex OTLP listener")?;
        listener
            .set_nonblocking(true)
            .context("configure Codex OTLP listener socket")?;
        let port = listener
            .local_addr()
            .context("read Codex OTLP listener address")?
            .port();
        let endpoint = format!("http://127.0.0.1:{port}/v1/logs");
        let state_path = top_listener_state_path()?;
        let state = TopListenerState {
            endpoint: endpoint.clone(),
            owner_pid: process::id(),
            updated_at_unix: current_unix_seconds(),
        };
        write_top_listener_state(&state_path, &state)?;

        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let (sender, receiver) = mpsc::channel();
        let join_handle = thread::spawn(move || {
            while !stop_flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = handle_otel_stream(stream, &sender);
                    }
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(100));
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            endpoint,
            receiver,
            state_path,
            state,
            stop,
            join_handle: Some(join_handle),
            last_heartbeat: SystemTime::now(),
        })
    }

    fn heartbeat(&mut self) -> Result<()> {
        let now = SystemTime::now();
        let elapsed = now.duration_since(self.last_heartbeat).unwrap_or_default();
        if elapsed < TOP_LISTENER_HEARTBEAT_INTERVAL {
            return Ok(());
        }
        self.state.updated_at_unix = current_unix_seconds();
        write_top_listener_state(&self.state_path, &self.state)?;
        self.last_heartbeat = now;
        Ok(())
    }

    fn shutdown(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(authority) = self
            .endpoint
            .strip_prefix("http://")
            .and_then(|value| value.split('/').next())
        {
            let _ = TcpStream::connect(authority);
        }
        if let Some(handle) = self.join_handle.take() {
            handle
                .join()
                .map_err(|_| anyhow!("join Codex OTLP listener thread"))?;
        }
        remove_top_listener_state(&self.state_path, &self.endpoint)
    }
}

impl Drop for TopListenerHandle {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn handle_otel_stream(stream: TcpStream, sender: &Sender<OtelSessionEvent>) -> Result<()> {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .context("set OTLP read timeout")?;
    let mut writer = stream
        .try_clone()
        .context("clone OTLP stream for response write")?;
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    if reader
        .read_line(&mut request_line)
        .context("read OTLP request line")?
        == 0
    {
        return Ok(());
    }

    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        let bytes = reader
            .read_line(&mut line)
            .context("read OTLP header line")?;
        if bytes == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            content_length = value
                .trim()
                .parse()
                .context("parse OTLP content-length header")?;
        }
    }

    let mut body = vec![0; content_length];
    reader
        .read_exact(&mut body)
        .context("read OTLP request body")?;
    for event in parse_otlp_session_events(&body)? {
        let _ = sender.send(event);
    }

    writer
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
        .context("write OTLP response")?;
    writer.flush().context("flush OTLP response")
}

pub(super) fn parse_otlp_session_events(body: &[u8]) -> Result<Vec<OtelSessionEvent>> {
    let payload =
        serde_json::from_slice::<OtlpLogsPayload>(body).context("parse OTLP JSON body")?;
    let mut events = Vec::new();
    for resource in payload.resource_logs {
        for scope in resource.scope_logs {
            for record in scope.log_records {
                let attributes = otlp_attributes_map(&record.attributes);
                let Some(session_id) = attributes.get("conversation.id").cloned() else {
                    continue;
                };
                let Some(event_name) = attributes.get("event.name").cloned() else {
                    continue;
                };
                let observed_unix =
                    parse_observed_unix_secs(record.observed_time_unix_nano.as_deref())
                        .unwrap_or_else(current_unix_seconds);
                events.push(OtelSessionEvent {
                    session_id,
                    event_name,
                    observed_unix,
                    model: attributes
                        .get("model")
                        .cloned()
                        .or_else(|| attributes.get("slug").cloned()),
                    effort: attributes.get("reasoning_effort").cloned(),
                    workspace_root: attributes
                        .get("cwd")
                        .cloned()
                        .or_else(|| attributes.get("workspace_root").cloned())
                        .or_else(|| attributes.get("workspace").cloned()),
                    tool_error: otlp_event_has_error(&record.attributes),
                    attributes,
                    body: record.body.as_ref().and_then(otlp_value_string),
                });
            }
        }
    }
    Ok(events)
}

fn parse_observed_unix_secs(value: Option<&str>) -> Option<u64> {
    let nanos = value?.trim().parse::<u128>().ok()?;
    Some((nanos / 1_000_000_000) as u64)
}

fn otlp_attr_string(attributes: &[OtlpAttribute], key: &str) -> Option<String> {
    attributes
        .iter()
        .find(|attribute| attribute.key == key)
        .and_then(|attribute| otlp_value_string(&attribute.value))
}

fn otlp_attr_bool(attributes: &[OtlpAttribute], key: &str) -> Option<bool> {
    let value = attributes.iter().find(|attribute| attribute.key == key)?;
    let object = value.value.as_object()?;
    object.get("boolValue").and_then(serde_json::Value::as_bool)
}

fn otlp_attributes_map(attributes: &[OtlpAttribute]) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for attribute in attributes {
        if let Some(value) = otlp_value_string(&attribute.value) {
            map.insert(attribute.key.clone(), value);
        }
    }
    map
}

fn otlp_value_string(value: &serde_json::Value) -> Option<String> {
    let object = value.as_object()?;
    if let Some(value) = object
        .get("stringValue")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(value.to_string());
    }
    if let Some(value) = object
        .get("intValue")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(value.to_string());
    }
    if let Some(value) = object.get("boolValue").and_then(serde_json::Value::as_bool) {
        return Some(value.to_string());
    }
    object
        .get("doubleValue")
        .and_then(serde_json::Value::as_f64)
        .map(|value| format!("{value}"))
}

fn otlp_event_has_error(attributes: &[OtlpAttribute]) -> bool {
    const ERROR_KEYS: [&str; 5] = [
        "error",
        "error.message",
        "tool.error",
        "tool_error",
        "exception.message",
    ];
    const SUCCESS_KEYS: [&str; 4] = ["success", "tool.success", "ok", "tool.ok"];

    if SUCCESS_KEYS
        .iter()
        .any(|key| otlp_attr_bool(attributes, key) == Some(false))
    {
        return true;
    }

    ERROR_KEYS.iter().any(|key| {
        otlp_attr_string(attributes, key)
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
    })
}

impl CodexTopApp {
    pub(super) fn new(current_workspace_root: PathBuf, show_all: bool, show_history: bool) -> Self {
        Self {
            current_workspace_root: current_workspace_root.display().to_string(),
            show_all,
            show_history,
            selected: 0,
            scroll_offset: 0,
            viewport_rows: 10,
            rows: Vec::new(),
            trackers: BTreeMap::new(),
            otel_state: OtelLiveState::default(),
            tmux_available: false,
            tmux_sessions: BTreeMap::new(),
            managed_records: Vec::new(),
            last_refresh: None,
            last_discovery: None,
            status_message: None,
            view: TopView::Summary,
        }
    }

    fn refresh(&mut self, listener: &mut TopListenerHandle) -> Result<()> {
        listener.heartbeat()?;
        let drained = self.drain_otel(listener);
        let now = SystemTime::now();
        let should_full_refresh = self
            .last_refresh
            .and_then(|last| now.duration_since(last).ok())
            .is_none_or(|elapsed| elapsed >= TOP_REFRESH_INTERVAL);
        if should_full_refresh {
            self.refresh_trackers(now)?;
            self.managed_records = load_session_records()?;
            match probe_tmux()? {
                TmuxProbe::Available => {
                    self.tmux_available = true;
                    self.tmux_sessions = list_tmux_sessions()?;
                }
                TmuxProbe::Missing => {
                    self.tmux_available = false;
                    self.tmux_sessions.clear();
                }
            }
            self.last_refresh = Some(now);
        }

        if drained || should_full_refresh || self.rows.is_empty() {
            let selected_key = self.rows.get(self.selected).map(|row| row.key.clone());
            self.rows = build_top_rows(TopRowsInput {
                current_workspace_root: &self.current_workspace_root,
                show_all: self.show_all,
                show_history: self.show_history,
                trackers: &self.trackers,
                otel_state: &self.otel_state,
                managed_records: &self.managed_records,
                tmux_available: self.tmux_available,
                tmux_sessions: &self.tmux_sessions,
            });
            if let Some(selected_key) = selected_key {
                if let Some(index) = self.rows.iter().position(|row| row.key == selected_key) {
                    self.selected = index;
                } else if self.selected >= self.rows.len() {
                    self.selected = self.rows.len().saturating_sub(1);
                }
            } else if self.selected >= self.rows.len() {
                self.selected = self.rows.len().saturating_sub(1);
            }
            if self.rows.is_empty() {
                self.scroll_offset = 0;
            }
        }
        self.rebind_stream_session_if_needed();
        Ok(())
    }

    fn refresh_trackers(&mut self, now: SystemTime) -> Result<()> {
        let should_discover = self
            .last_discovery
            .and_then(|last| now.duration_since(last).ok())
            .is_none_or(|elapsed| elapsed >= TOP_DISCOVERY_INTERVAL);
        if should_discover {
            let paths = discover_codex_session_paths()?;
            let wanted = paths.iter().cloned().collect::<BTreeSet<_>>();
            for path in paths {
                self.trackers
                    .entry(path.clone())
                    .or_insert_with(|| SessionFileTracker::new(path));
            }
            self.trackers.retain(|path, _| wanted.contains(path));
            self.last_discovery = Some(now);
        }

        for tracker in self.trackers.values_mut() {
            tracker.sync()?;
        }
        Ok(())
    }

    fn drain_otel(&mut self, listener: &mut TopListenerHandle) -> bool {
        let mut changed = false;
        loop {
            match listener.receiver.try_recv() {
                Ok(event) => {
                    self.apply_otel_event(event);
                    changed = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.status_message = Some("live OTLP listener disconnected".to_string());
                    break;
                }
            }
        }
        changed
    }

    fn apply_otel_event(&mut self, event: OtelSessionEvent) {
        let session = self
            .otel_state
            .sessions
            .entry(event.session_id.clone())
            .or_default();
        if let Some(model) = event.model.filter(|value| !value.is_empty()) {
            session.model = Some(model);
        }
        if let Some(effort) = event.effort.filter(|value| !value.is_empty()) {
            session.effort = Some(effort);
        }
        if let Some(workspace_root) = event.workspace_root.filter(|value| !value.is_empty()) {
            session.workspace_root = Some(workspace_root);
        }
        session.last_activity_unix = Some(
            session
                .last_activity_unix
                .unwrap_or_default()
                .max(event.observed_unix),
        );
        session.last_event_name = Some(event.event_name.clone());
        session.otel_events += 1;
        if event.event_name.ends_with("api_request") {
            session.api_requests += 1;
        }
        if event.event_name.ends_with("sse_event") {
            session.sse_events += 1;
        }
        if event.event_name.ends_with("tool_result")
            || event.event_name.ends_with("tool_call")
            || event.event_name.contains(".tool_")
        {
            session.tool_calls += 1;
            if event.tool_error {
                session.tool_errors += 1;
            }
        }
        self.otel_state.total_events += 1;
        self.otel_state.last_event_unix = Some(
            self.otel_state
                .last_event_unix
                .unwrap_or_default()
                .max(event.observed_unix),
        );

        let session_events = self
            .otel_state
            .session_events
            .entry(event.session_id.clone())
            .or_default();
        session_events.push_back(OtelEventRecord {
            observed_unix: event.observed_unix,
            event_name: event.event_name.clone(),
            tool_error: event.tool_error,
            attributes: event.attributes,
            body: event.body,
        });
        while session_events.len() > TOP_STREAM_EVENT_CAP {
            session_events.pop_front();
        }

        if let TopView::Stream(stream) = &mut self.view
            && stream.follow
            && stream.session_id == event.session_id
        {
            stream.selected = session_events.len().saturating_sub(1);
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        if delta.is_negative() {
            self.selected = self.selected.saturating_sub(delta.unsigned_abs());
        } else {
            self.selected = self
                .selected
                .saturating_add(delta as usize)
                .min(self.rows.len().saturating_sub(1));
        }
    }

    fn move_to_start(&mut self) {
        self.selected = 0;
    }

    fn move_to_end(&mut self) {
        self.selected = self.rows.len().saturating_sub(1);
    }

    fn page_down(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let step = self.viewport_rows.saturating_sub(1).max(1);
        self.selected = self
            .selected
            .saturating_add(step)
            .min(self.rows.len().saturating_sub(1));
    }

    fn page_up(&mut self) {
        let step = self.viewport_rows.saturating_sub(1).max(1);
        self.selected = self.selected.saturating_sub(step);
    }

    fn toggle_scope(&mut self) {
        self.show_all = !self.show_all;
        self.selected = 0;
        self.scroll_offset = 0;
        self.status_message = Some(if self.show_all {
            "scope switched to all local Codex sessions".to_string()
        } else {
            "scope switched to current workspace".to_string()
        });
    }

    fn toggle_history(&mut self) {
        self.show_history = !self.show_history;
        self.selected = 0;
        self.scroll_offset = 0;
        self.status_message = Some(if self.show_history {
            "history rows enabled".to_string()
        } else {
            "history rows hidden; showing active sessions only".to_string()
        });
    }

    fn handle_key(&mut self, code: KeyCode) {
        if matches!(self.view, TopView::Summary) {
            self.handle_summary_key(code);
        } else {
            self.handle_stream_key(code);
        }
    }

    fn handle_summary_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {}
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Home | KeyCode::Char('g') => self.move_to_start(),
            KeyCode::End | KeyCode::Char('G') => self.move_to_end(),
            KeyCode::PageDown => self.page_down(),
            KeyCode::PageUp => self.page_up(),
            KeyCode::Char('a') => self.toggle_scope(),
            KeyCode::Char('h') => self.toggle_history(),
            KeyCode::Enter => self.open_selected_stream(),
            _ => {}
        }
    }

    fn handle_stream_key(&mut self, code: KeyCode) {
        let (session_id, viewport_rows, selected, follow) = match &self.view {
            TopView::Stream(stream) => (
                stream.session_id.clone(),
                stream.viewport_rows,
                stream.selected,
                stream.follow,
            ),
            TopView::Summary => return,
        };
        let event_len = self.stream_event_len(&session_id);

        match code {
            KeyCode::Esc | KeyCode::Backspace => self.view = TopView::Summary,
            KeyCode::Down | KeyCode::Char('j') => {
                self.update_stream_state(|stream| {
                    stream.follow = false;
                    stream.selected = selected.saturating_add(1).min(event_len.saturating_sub(1));
                });
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.update_stream_state(|stream| {
                    stream.follow = false;
                    stream.selected = selected.saturating_sub(1);
                });
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.update_stream_state(|stream| {
                    stream.follow = false;
                    stream.selected = 0;
                });
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.update_stream_state(|stream| {
                    stream.follow = true;
                    stream.selected = event_len.saturating_sub(1);
                });
            }
            KeyCode::PageDown => {
                let step = viewport_rows.saturating_sub(1).max(1);
                self.update_stream_state(|stream| {
                    stream.follow = false;
                    stream.selected = selected
                        .saturating_add(step)
                        .min(event_len.saturating_sub(1));
                });
            }
            KeyCode::PageUp => {
                let step = viewport_rows.saturating_sub(1).max(1);
                self.update_stream_state(|stream| {
                    stream.follow = false;
                    stream.selected = selected.saturating_sub(step);
                });
            }
            KeyCode::Char('f') => {
                let next_follow = !follow;
                self.update_stream_state(|stream| {
                    stream.follow = next_follow;
                    if next_follow {
                        stream.selected = event_len.saturating_sub(1);
                    }
                });
                self.status_message = Some(if next_follow {
                    "stream follow enabled".to_string()
                } else {
                    "stream follow paused".to_string()
                });
            }
            _ => {}
        }
    }

    fn open_selected_stream(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        let Some(session_id) = row.session_id.clone() else {
            self.status_message = Some(
                "selected row has no Codex conversation id; cannot open OTel stream".to_string(),
            );
            return;
        };
        let selected = self.stream_event_len(&session_id).saturating_sub(1);
        self.view = TopView::Stream(TopStreamState {
            session_id,
            workspace_root: row.workspace_root.clone(),
            model: row.model.clone(),
            effort: row.effort.clone(),
            tmux_running: row.tmux_running,
            live_otel: row.live_otel,
            selected,
            scroll_offset: 0,
            viewport_rows: 10,
            follow: true,
        });
    }

    pub(super) fn rebind_stream_session_if_needed(&mut self) {
        let Some((next_session_id, next_model, next_effort, next_tmux_running, next_live_otel)) =
            (match &self.view {
                TopView::Summary => None,
                TopView::Stream(stream) => preferred_stream_row(
                    &self.rows,
                    &stream.session_id,
                    &stream.workspace_root,
                    &self.otel_state,
                )
                .and_then(|row| {
                    row.session_id.as_ref().map(|session_id| {
                        (
                            session_id.clone(),
                            row.model.clone(),
                            row.effort.clone(),
                            row.tmux_running,
                            row.live_otel,
                        )
                    })
                }),
            })
        else {
            return;
        };
        let next_event_len = self.stream_event_len(&next_session_id);
        self.update_stream_state(|stream| {
            if stream.session_id == next_session_id {
                return;
            }
            stream.session_id = next_session_id.clone();
            stream.model = next_model.clone();
            stream.effort = next_effort.clone();
            stream.tmux_running = next_tmux_running;
            stream.live_otel = next_live_otel;
            stream.selected = if stream.follow {
                next_event_len.saturating_sub(1)
            } else {
                stream.selected.min(next_event_len.saturating_sub(1))
            };
            stream.scroll_offset = 0;
        });
        self.status_message = Some(format!(
            "stream rebound to live OTel session {}",
            truncate_end(&next_session_id, 12)
        ));
    }

    fn stream_event_len(&self, session_id: &str) -> usize {
        self.stream_event_vec(session_id).len()
    }

    fn update_stream_state(&mut self, update: impl FnOnce(&mut TopStreamState)) {
        if let TopView::Stream(stream) = &mut self.view {
            update(stream);
        }
    }

    fn stream_event_vec(&self, session_id: &str) -> Vec<OtelEventRecord> {
        self.otel_state
            .session_events
            .get(session_id)
            .map(|events| events.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default()
    }
}

pub(super) fn build_top_rows(input: TopRowsInput<'_>) -> Vec<CodexTopRow> {
    let TopRowsInput {
        current_workspace_root,
        show_all,
        show_history,
        trackers,
        otel_state,
        managed_records,
        tmux_available,
        tmux_sessions,
    } = input;
    let mut rows = Vec::new();
    let mut seen_keys = BTreeSet::new();
    let mut managed_assignments = BTreeMap::new();
    let mut synthetic_records = Vec::new();
    let visible_trackers = trackers
        .values()
        .filter(|tracker| {
            workspace_visible(
                show_all,
                current_workspace_root,
                tracker.state.workspace_root.as_deref(),
            )
        })
        .collect::<Vec<_>>();
    let mut assigned_tracker_keys = BTreeSet::new();

    let mut visible_records = managed_records
        .iter()
        .filter(|record| {
            workspace_visible(
                show_all,
                current_workspace_root,
                Some(&record.workspace_root),
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    visible_records.sort_by_key(|record| record.created_at_unix);

    for record in visible_records {
        if let Some(tracker_key) =
            best_tracker_match_for_record(&record, &visible_trackers, &assigned_tracker_keys)
        {
            assigned_tracker_keys.insert(tracker_key.clone());
            managed_assignments.insert(tracker_key, record.clone());
        } else {
            synthetic_records.push(record.clone());
        }
    }

    for tracker in trackers.values() {
        let workspace_root = tracker.state.workspace_root.clone().or_else(|| {
            tracker
                .state
                .session_id
                .as_ref()
                .and_then(|id| otel_state.sessions.get(id))
                .and_then(|session| session.workspace_root.clone())
        });
        if !workspace_visible(show_all, current_workspace_root, workspace_root.as_deref()) {
            continue;
        }

        let key = tracker.key();
        let tracker_otel = tracker.state.session_id.as_ref().and_then(|session_id| {
            otel_state
                .sessions
                .get(session_id)
                .map(|session| (session_id, session))
        });
        let workspace_otel = workspace_root
            .as_deref()
            .and_then(|workspace_root| latest_workspace_otel_session(otel_state, workspace_root));
        let otel = pick_preferred_row_otel(tracker_otel, workspace_otel);
        let managed_record = managed_assignments.get(&key);
        let tmux = managed_record.and_then(|record| tmux_sessions.get(&record.session_name));
        let tmux_running = tmux.is_some();
        let live_otel = otel
            .and_then(|(_, session)| session.last_activity_unix)
            .is_some_and(|last| current_unix_seconds().saturating_sub(last) <= 5);
        let last_activity_unix = select_latest_activity(
            tracker
                .state
                .last_activity_unix
                .or(Some(tracker.modified_unix)),
            otel.and_then(|(_, session)| session.last_activity_unix),
        );
        let status = top_row_status(
            tmux_running,
            managed_record.is_some(),
            tmux_available,
            live_otel,
            last_activity_unix,
        )
        .to_string();

        let row_session_id = otel
            .map(|(session_id, _)| session_id.clone())
            .or_else(|| tracker.state.session_id.clone());
        rows.push(CodexTopRow {
            key: key.clone(),
            session_id: row_session_id.clone(),
            managed_session_name: managed_record.map(|record| record.session_name.clone()),
            workspace_root: workspace_root.unwrap_or_else(|| tracker.path.display().to_string()),
            model: tracker
                .state
                .model
                .clone()
                .or_else(|| otel.and_then(|(_, session)| session.model.clone())),
            effort: tracker
                .state
                .effort
                .clone()
                .or_else(|| otel.and_then(|(_, session)| session.effort.clone())),
            context_left_percent: tracker.state.context_left_percent,
            status,
            tmux_running,
            attached_clients: tmux.map(|info| info.attached_clients).unwrap_or(0),
            last_activity_unix,
            last_event_name: choose_latest_event_name(
                tracker
                    .state
                    .last_activity_unix
                    .or(Some(tracker.modified_unix)),
                tracker.state.last_event_name.as_deref(),
                otel.and_then(|(_, session)| session.last_activity_unix),
                otel.and_then(|(_, session)| session.last_event_name.as_deref()),
            ),
            otel_events: otel
                .map(|(_, session)| session.otel_events)
                .unwrap_or_default(),
            api_requests: otel
                .map(|(_, session)| session.api_requests)
                .unwrap_or_default(),
            live_tool_calls: otel
                .map(|(_, session)| session.tool_calls)
                .unwrap_or_default(),
            lifetime_tool_calls: tracker.state.tool_calls,
            live_tool_errors: otel
                .map(|(_, session)| session.tool_errors)
                .unwrap_or_default(),
            lifetime_tool_errors: tracker.state.tool_errors,
            sse_events: otel
                .map(|(_, session)| session.sse_events)
                .unwrap_or_default(),
            live_otel,
        });
        seen_keys.insert(key);
        if let Some(session_id) = row_session_id {
            seen_keys.insert(session_id);
        }
    }

    for record in synthetic_records {
        let tmux = tmux_sessions.get(&record.session_name);
        let tmux_running = tmux.is_some();
        let otel = latest_workspace_otel_session(otel_state, &record.workspace_root);
        let row_session_id = otel.map(|(session_id, _)| session_id.clone());
        rows.push(CodexTopRow {
            key: format!("managed:{}", record.session_name),
            session_id: row_session_id.clone(),
            managed_session_name: Some(record.session_name.clone()),
            workspace_root: record.workspace_root.clone(),
            model: otel.and_then(|(_, session)| session.model.clone()),
            effort: otel.and_then(|(_, session)| session.effort.clone()),
            context_left_percent: None,
            status: session_status_label(tmux_running, true, tmux_available),
            tmux_running,
            attached_clients: tmux.map(|info| info.attached_clients).unwrap_or(0),
            last_activity_unix: select_latest_activity(
                tmux.and_then(|info| info.activity_unix)
                    .or(Some(record.created_at_unix)),
                otel.and_then(|(_, session)| session.last_activity_unix),
            ),
            last_event_name: choose_latest_event_name(
                tmux.and_then(|info| info.activity_unix)
                    .or(Some(record.created_at_unix)),
                Some(&format!("launcher:{}", record.launcher)),
                otel.and_then(|(_, session)| session.last_activity_unix),
                otel.and_then(|(_, session)| session.last_event_name.as_deref()),
            ),
            otel_events: otel
                .map(|(_, session)| session.otel_events)
                .unwrap_or_default(),
            api_requests: otel
                .map(|(_, session)| session.api_requests)
                .unwrap_or_default(),
            live_tool_calls: otel
                .map(|(_, session)| session.tool_calls)
                .unwrap_or_default(),
            lifetime_tool_calls: 0,
            live_tool_errors: otel
                .map(|(_, session)| session.tool_errors)
                .unwrap_or_default(),
            lifetime_tool_errors: 0,
            sse_events: otel
                .map(|(_, session)| session.sse_events)
                .unwrap_or_default(),
            live_otel: otel
                .and_then(|(_, session)| session.last_activity_unix)
                .is_some_and(|last| current_unix_seconds().saturating_sub(last) <= 5),
        });
        if let Some(session_id) = row_session_id {
            seen_keys.insert(session_id);
        }
    }

    for (session_id, otel) in &otel_state.sessions {
        if seen_keys.contains(session_id) {
            continue;
        }
        if !workspace_visible(
            show_all,
            current_workspace_root,
            otel.workspace_root.as_deref(),
        ) {
            continue;
        }
        rows.push(CodexTopRow {
            key: session_id.clone(),
            session_id: Some(session_id.clone()),
            managed_session_name: None,
            workspace_root: otel
                .workspace_root
                .clone()
                .unwrap_or_else(|| "<unknown workspace>".to_string()),
            model: otel.model.clone(),
            effort: otel.effort.clone(),
            context_left_percent: None,
            status: top_row_status(false, false, tmux_available, true, otel.last_activity_unix)
                .to_string(),
            tmux_running: false,
            attached_clients: 0,
            last_activity_unix: otel.last_activity_unix,
            last_event_name: otel.last_event_name.clone(),
            otel_events: otel.otel_events,
            api_requests: otel.api_requests,
            live_tool_calls: otel.tool_calls,
            lifetime_tool_calls: 0,
            live_tool_errors: otel.tool_errors,
            lifetime_tool_errors: 0,
            sse_events: otel.sse_events,
            live_otel: true,
        });
    }

    if !show_history {
        rows.retain(row_is_active_now);
    }

    rows.sort_by(|a, b| {
        top_status_rank(&a.status)
            .cmp(&top_status_rank(&b.status))
            .then_with(|| b.last_activity_unix.cmp(&a.last_activity_unix))
            .then_with(|| a.workspace_root.cmp(&b.workspace_root))
            .then_with(|| a.key.cmp(&b.key))
    });
    rows
}

fn latest_workspace_otel_session<'a>(
    otel_state: &'a OtelLiveState,
    workspace_root: &str,
) -> Option<(&'a String, &'a OtelSessionState)> {
    otel_state
        .sessions
        .iter()
        .filter(|(_, session)| session.workspace_root.as_deref() == Some(workspace_root))
        .max_by(|(left_id, left), (right_id, right)| {
            left.last_activity_unix
                .cmp(&right.last_activity_unix)
                .then_with(|| left.otel_events.cmp(&right.otel_events))
                .then_with(|| left_id.cmp(right_id))
        })
}

fn pick_preferred_row_otel<'a>(
    tracker_otel: Option<(&'a String, &'a OtelSessionState)>,
    workspace_otel: Option<(&'a String, &'a OtelSessionState)>,
) -> Option<(&'a String, &'a OtelSessionState)> {
    match (tracker_otel, workspace_otel) {
        (Some((_, tracker)), Some((workspace_id, workspace)))
            if workspace.last_activity_unix >= tracker.last_activity_unix =>
        {
            Some((workspace_id, workspace))
        }
        (Some((tracker_id, tracker)), Some(_)) => Some((tracker_id, tracker)),
        (None, Some((workspace_id, workspace))) => Some((workspace_id, workspace)),
        (Some((tracker_id, tracker)), None) => Some((tracker_id, tracker)),
        (None, None) => None,
    }
}

fn preferred_stream_row<'a>(
    rows: &'a [CodexTopRow],
    current_session_id: &str,
    workspace_root: &str,
    otel_state: &OtelLiveState,
) -> Option<&'a CodexTopRow> {
    if otel_state.sessions.contains_key(current_session_id) {
        return None;
    }

    rows.iter()
        .filter(|row| row.workspace_root == workspace_root)
        .filter(|row| row.session_id.as_deref() != Some(current_session_id))
        .filter(|row| row.session_id.is_some())
        .filter(|row| row.otel_events > 0 || row.live_otel)
        .max_by(|left, right| {
            left.live_otel
                .cmp(&right.live_otel)
                .then_with(|| left.last_activity_unix.cmp(&right.last_activity_unix))
                .then_with(|| left.otel_events.cmp(&right.otel_events))
                .then_with(|| left.key.cmp(&right.key))
        })
}

pub(super) fn best_tracker_match_for_record(
    record: &SessionRecord,
    trackers: &[&SessionFileTracker],
    assigned_tracker_keys: &BTreeSet<String>,
) -> Option<String> {
    let candidates = trackers
        .iter()
        .filter(|tracker| {
            tracker.state.workspace_root.as_deref() == Some(record.workspace_root.as_str())
        })
        .filter(|tracker| !assigned_tracker_keys.contains(&tracker.key()))
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return None;
    }

    let created_at_unix = record.created_at_unix;

    if let Some((tracker_key, _, _)) = candidates
        .iter()
        .filter_map(|tracker| {
            let tracker_key = tracker.key();
            let reference_unix = tracker_match_reference_unix(tracker);
            let delta = reference_unix.checked_sub(created_at_unix)?;
            (delta <= MANAGED_TRACKER_MATCH_WINDOW_SECS).then_some((
                tracker_key,
                delta,
                reference_unix,
            ))
        })
        .min_by(|a, b| {
            a.1.cmp(&b.1)
                .then_with(|| b.2.cmp(&a.2))
                .then_with(|| a.0.cmp(&b.0))
        })
    {
        return Some(tracker_key);
    }

    if let Some((tracker_key, _, _)) = candidates
        .iter()
        .filter_map(|tracker| {
            let tracker_key = tracker.key();
            let reference_unix = tracker_match_reference_unix(tracker);
            let diff = reference_unix.abs_diff(created_at_unix);
            (diff <= MANAGED_TRACKER_MATCH_WINDOW_SECS).then_some((
                tracker_key,
                diff,
                reference_unix,
            ))
        })
        .min_by(|a, b| {
            a.1.cmp(&b.1)
                .then_with(|| b.2.cmp(&a.2))
                .then_with(|| a.0.cmp(&b.0))
        })
    {
        return Some(tracker_key);
    }

    candidates
        .iter()
        .map(|tracker| (tracker.key(), tracker_match_reference_unix(tracker)))
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
        .map(|(tracker_key, _)| tracker_key)
}

fn tracker_match_reference_unix(tracker: &SessionFileTracker) -> u64 {
    tracker
        .state
        .started_unix
        .unwrap_or_default()
        .max(tracker.state.last_activity_unix.unwrap_or_default())
        .max(tracker.modified_unix)
}

fn workspace_visible(
    show_all: bool,
    current_workspace_root: &str,
    workspace_root: Option<&str>,
) -> bool {
    if show_all {
        return true;
    }
    workspace_root == Some(current_workspace_root)
}

fn top_row_status(
    tmux_running: bool,
    managed: bool,
    tmux_available: bool,
    live_otel: bool,
    last_activity_unix: Option<u64>,
) -> &'static str {
    if tmux_running {
        return "running";
    }
    if live_otel {
        return "live";
    }
    if managed && !tmux_available {
        return "unavailable";
    }
    if managed {
        return "stale";
    }
    let elapsed = last_activity_unix.map(|unix| current_unix_seconds().saturating_sub(unix));
    if elapsed.is_some_and(|elapsed| elapsed <= 60) {
        "idle"
    } else {
        "ended"
    }
}

fn top_status_rank(status: &str) -> usize {
    match status {
        "running" => 0,
        "live" => 1,
        "idle" => 2,
        "stale" => 3,
        "unavailable" => 4,
        _ => 5,
    }
}

fn row_is_active_now(row: &CodexTopRow) -> bool {
    row.tmux_running || row.live_otel
}

fn select_latest_activity(file_activity: Option<u64>, otel_activity: Option<u64>) -> Option<u64> {
    match (file_activity, otel_activity) {
        (Some(file), Some(otel)) => Some(file.max(otel)),
        (Some(file), None) => Some(file),
        (None, Some(otel)) => Some(otel),
        (None, None) => None,
    }
}

fn choose_latest_event_name(
    file_activity: Option<u64>,
    file_name: Option<&str>,
    otel_activity: Option<u64>,
    otel_name: Option<&str>,
) -> Option<String> {
    match (file_activity, file_name, otel_activity, otel_name) {
        (_, _, Some(otel_activity), Some(otel_name))
            if otel_activity >= file_activity.unwrap_or_default() =>
        {
            Some(otel_name.to_string())
        }
        (_, Some(file_name), _, _) => Some(file_name.to_string()),
        (_, _, _, Some(otel_name)) => Some(otel_name.to_string()),
        _ => None,
    }
}

fn draw_top_tui(
    frame: &mut ratatui::Frame<'_>,
    app: &mut CodexTopApp,
    listener: &TopListenerHandle,
) {
    if matches!(app.view, TopView::Stream(_)) {
        draw_stream_tui(frame, app, listener);
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(8),
            Constraint::Length(8),
        ])
        .split(frame.area());

    let live_rows = app
        .rows
        .iter()
        .filter(|row| matches!(row.status.as_str(), "running" | "live"))
        .count();
    let overview = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                "za codex top",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::raw(format!(
                "scope={}  history={}  visible={}  live={}  total-otel={}",
                if app.show_all { "all" } else { "workspace" },
                if app.show_history { "on" } else { "off" },
                app.rows.len(),
                live_rows,
                app.otel_state.total_events
            )),
        ]),
        Line::from(Span::raw(format!("listener={}", listener.endpoint))),
        Line::from(Span::raw(format!(
            "last-otel={}  current-workspace={}",
            activity_age_label(app.otel_state.last_event_unix),
            truncate_end(
                &app.current_workspace_root,
                usize::from(chunks[0].width.saturating_sub(4)).max(1)
            ),
        ))),
    ])
    .block(Block::default().borders(Borders::ALL).title("Overview"));
    frame.render_widget(overview, chunks[0]);

    let sessions_block = Block::default().borders(Borders::ALL).title("Sessions");
    let inner = sessions_block.inner(chunks[1]);
    frame.render_widget(sessions_block, chunks[1]);

    let session_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);
    let header = Paragraph::new(top_rows_header_line()).style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_widget(header, session_chunks[0]);

    let items = if app.rows.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            if app.show_history {
                "No Codex sessions matched the current scope."
            } else {
                "No active Codex sessions matched the current scope. Press `h` to include history."
            },
            Style::default().fg(Color::DarkGray),
        )))]
    } else {
        app.rows.iter().map(top_row_item).collect::<Vec<_>>()
    };
    let mut list_state = ListState::default()
        .with_offset(app.scroll_offset)
        .with_selected((!app.rows.is_empty()).then_some(app.selected));
    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, session_chunks[1], &mut list_state);
    app.scroll_offset = list_state.offset();
    app.viewport_rows = usize::from(session_chunks[1].height.max(1));

    let detail = match app.rows.get(app.selected) {
        Some(row) => top_detail_lines(row, &app.status_message),
        None => vec![
            Line::from("j/k move  PgUp/PgDn page  Enter stream  a scope  h history  q quit"),
            Line::from(app.status_message.clone().unwrap_or_else(|| {
                "Launching `za codex` while this screen is open will auto-enable live OTLP streaming.".to_string()
            })),
        ],
    };
    let detail =
        Paragraph::new(detail).block(Block::default().borders(Borders::ALL).title("Detail"));
    frame.render_widget(detail, chunks[2]);
}

fn top_rows_header_line() -> Line<'static> {
    Line::from(format!(
        "{:<4} {:<4} {:<6} {:<5} {:<18} {:>3} {:>5} {:>5} {:>7} {:<12} {}",
        "TMUX",
        "LIVE",
        "ACTIVE",
        "LEFT",
        "MODEL/EFFORT",
        "API",
        "TLIVE",
        "TLIFE",
        "ERR L/A",
        "SESSION",
        "WORKSPACE"
    ))
}

fn top_row_item(row: &CodexTopRow) -> ListItem<'static> {
    let model = match (&row.model, &row.effort) {
        (Some(model), Some(effort)) => format!("{model}/{effort}"),
        (Some(model), None) => model.clone(),
        (None, Some(effort)) => format!("-/{effort}"),
        (None, None) => "-".to_string(),
    };
    let tmux_label = if row.tmux_running { "yes" } else { "-" };
    let live_label = if row.live_otel { "yes" } else { "-" };
    let err_label = format!("{}/{}", row.live_tool_errors, row.lifetime_tool_errors);
    let line = format!(
        "{:<4} {:<4} {:<6} {:<5} {:<18} {:>3} {:>5} {:>5} {:>7} {:<12} {}",
        tmux_label,
        live_label,
        activity_age_label(row.last_activity_unix),
        format_left_percent(row.context_left_percent),
        truncate_end(&model, 18),
        row.api_requests,
        row.live_tool_calls,
        row.lifetime_tool_calls,
        err_label,
        truncate_end(row.session_id.as_deref().unwrap_or("-"), 12),
        truncate_end(&row.workspace_root, 80),
    );
    ListItem::new(Line::from(vec![
        Span::styled(
            format!("{:<4}", tmux_label),
            top_status_style(&row.status, row.live_otel),
        ),
        Span::raw(line[4..].to_string()),
    ]))
}

fn top_status_style(status: &str, live_otel: bool) -> Style {
    let base = match status {
        "running" => Style::default().fg(Color::Green),
        "live" => Style::default().fg(Color::Cyan),
        "idle" => Style::default().fg(Color::Yellow),
        "stale" | "unavailable" => Style::default().fg(Color::Red),
        _ => Style::default().fg(Color::DarkGray),
    };
    if live_otel {
        base.add_modifier(Modifier::BOLD)
    } else {
        base
    }
}

fn top_detail_lines(row: &CodexTopRow, status_message: &Option<String>) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(Line::from(format!(
        "status={}  tmux={}  live={}  session={}  managed={}",
        row.status,
        if row.tmux_running { "yes" } else { "no" },
        if row.live_otel { "yes" } else { "no" },
        row.session_id.as_deref().unwrap_or("-"),
        row.managed_session_name.as_deref().unwrap_or("-"),
    )));
    lines.push(Line::from(format!(
        "workspace={}  clients={}  last={}",
        row.workspace_root,
        row.attached_clients,
        row.last_event_name.as_deref().unwrap_or("-"),
    )));
    lines.push(Line::from(format!(
        "model={}  left={}  api={}  otel={}  sse={}",
        row.model.as_deref().unwrap_or("-"),
        format_left_percent(row.context_left_percent),
        row.api_requests,
        row.otel_events,
        row.sse_events,
    )));
    lines.push(Line::from(format!(
        "tool_live={}  tool_life={}  err_live={}  err_life={}  Enter stream  a scope  h history  q quit",
        row.live_tool_calls,
        row.lifetime_tool_calls,
        row.live_tool_errors,
        row.lifetime_tool_errors,
    )));
    lines.push(Line::from(format!(
        "effort={}",
        row.effort.as_deref().unwrap_or("-"),
    )));
    if let Some(message) = status_message {
        lines.push(Line::from(message.clone()));
    }
    lines
}

fn draw_stream_tui(
    frame: &mut ratatui::Frame<'_>,
    app: &mut CodexTopApp,
    listener: &TopListenerHandle,
) {
    let (
        session_id,
        workspace_root,
        model,
        effort,
        tmux_running,
        live_otel,
        follow,
        scroll_offset,
        selected,
    ) = match &app.view {
        TopView::Stream(stream) => (
            stream.session_id.clone(),
            stream.workspace_root.clone(),
            stream.model.clone(),
            stream.effort.clone(),
            stream.tmux_running,
            stream.live_otel,
            stream.follow,
            stream.scroll_offset,
            stream.selected,
        ),
        TopView::Summary => return,
    };

    let events = app.stream_event_vec(&session_id);
    let resolved_selected = if follow {
        events.len().saturating_sub(1)
    } else {
        selected.min(events.len().saturating_sub(1))
    };
    app.update_stream_state(|stream| {
        stream.selected = resolved_selected;
    });

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(8),
            Constraint::Length(8),
        ])
        .split(frame.area());

    let summary_row = app
        .rows
        .iter()
        .find(|row| row.session_id.as_deref() == Some(session_id.as_str()));
    let overview = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                "za codex top / stream",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::raw(format!(
                "events={}  follow={}  listener={}",
                events.len(),
                if follow { "on" } else { "off" },
                truncate_end(&listener.endpoint, 24)
            )),
        ]),
        Line::from(Span::raw(format!(
            "session={}  workspace={}",
            session_id,
            summary_row
                .map(|row| row.workspace_root.as_str())
                .unwrap_or(workspace_root.as_str())
        ))),
        Line::from(Span::raw(format!(
            "model={}  effort={}  tmux={}  live={}",
            summary_row
                .and_then(|row| row.model.as_deref())
                .unwrap_or(model.as_deref().unwrap_or("-")),
            summary_row
                .and_then(|row| row.effort.as_deref())
                .unwrap_or(effort.as_deref().unwrap_or("-")),
            if summary_row.is_some_and(|row| row.tmux_running)
                || summary_row.is_none() && tmux_running
            {
                "yes"
            } else {
                "no"
            },
            if summary_row.is_some_and(|row| row.live_otel) || summary_row.is_none() && live_otel {
                "yes"
            } else {
                "no"
            }
        ))),
    ])
    .block(Block::default().borders(Borders::ALL).title("Event Stream"));
    frame.render_widget(overview, chunks[0]);

    let stream_block = Block::default().borders(Borders::ALL).title("Events");
    let inner = stream_block.inner(chunks[1]);
    frame.render_widget(stream_block, chunks[1]);
    let stream_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);
    let header = Paragraph::new(stream_rows_header_line()).style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_widget(header, stream_chunks[0]);

    let items = if events.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "No live OTel events captured for this session yet.",
            Style::default().fg(Color::DarkGray),
        )))]
    } else {
        events
            .iter()
            .map(|event| stream_row_item(event, stream_chunks[1].width))
            .collect::<Vec<_>>()
    };
    let mut list_state = ListState::default()
        .with_offset(scroll_offset)
        .with_selected((!events.is_empty()).then_some(resolved_selected));
    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, stream_chunks[1], &mut list_state);
    app.update_stream_state(|stream| {
        stream.scroll_offset = list_state.offset();
        stream.viewport_rows = usize::from(stream_chunks[1].height.max(1));
    });

    let detail = events
        .get(resolved_selected)
        .map(|event| stream_detail_lines(event, &app.status_message))
        .unwrap_or_else(|| {
            vec![
                Line::from("Esc back  f follow  j/k move  PgUp/PgDn page  q quit"),
                Line::from(app.status_message.clone().unwrap_or_else(|| {
                    "Waiting for the selected session to emit new OTel events.".to_string()
                })),
            ]
        });
    let detail =
        Paragraph::new(detail).block(Block::default().borders(Borders::ALL).title("Event Detail"));
    frame.render_widget(detail, chunks[2]);
}

fn stream_rows_header_line() -> Line<'static> {
    Line::from(format!(
        "{:<6} {:<5} {:<28} {}",
        "ACTIVE", "ERR", "EVENT", "ATTRS"
    ))
}

fn stream_row_item(event: &OtelEventRecord, width: u16) -> ListItem<'static> {
    let snippet_width = usize::from(width.saturating_sub(2)).saturating_sub(44);
    let snippet = truncate_end(&stream_event_snippet(event), snippet_width.max(12));
    let line = format!(
        "{:<6} {:<5} {:<28} {}",
        activity_age_label(Some(event.observed_unix)),
        if event.tool_error { "yes" } else { "-" },
        truncate_end(&event.event_name, 28),
        snippet,
    );
    ListItem::new(Line::from(vec![
        Span::styled(
            format!("{:<6}", activity_age_label(Some(event.observed_unix))),
            if event.tool_error {
                Style::default().fg(Color::Red)
            } else {
                Style::default()
            },
        ),
        Span::raw(line[6..].to_string()),
    ]))
}

fn stream_event_snippet(event: &OtelEventRecord) -> String {
    let mut fields = Vec::new();
    for (key, value) in &event.attributes {
        if matches!(
            key.as_str(),
            "conversation.id"
                | "event.name"
                | "event.timestamp"
                | "model"
                | "slug"
                | "reasoning_effort"
                | "cwd"
                | "workspace"
                | "workspace_root"
        ) {
            continue;
        }
        fields.push(format!("{key}={value}"));
        if fields.len() >= 3 {
            break;
        }
    }
    if fields.is_empty() {
        event.body.clone().unwrap_or_else(|| "-".to_string())
    } else {
        fields.join("  ")
    }
}

fn stream_detail_lines(
    event: &OtelEventRecord,
    status_message: &Option<String>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(Line::from(format!(
        "event={}  active={}  error={}  attrs={}  Esc back  f follow  q quit",
        event.event_name,
        activity_age_label(Some(event.observed_unix)),
        if event.tool_error { "yes" } else { "no" },
        event.attributes.len(),
    )));
    if let Some(body) = &event.body {
        lines.push(Line::from(format!("body={}", truncate_end(body, 120))));
    }
    let mut attr_lines = Vec::new();
    for (key, value) in &event.attributes {
        attr_lines.push(format!("{key}={value}"));
    }
    if attr_lines.is_empty() {
        lines.push(Line::from("attributes: -"));
    } else {
        for chunk in attr_lines.chunks(2).take(4) {
            lines.push(Line::from(chunk.join("    ")));
        }
    }
    if let Some(message) = status_message {
        lines.push(Line::from(message.clone()));
    }
    lines
}
