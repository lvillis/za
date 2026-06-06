use super::*;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    time::{Duration, Instant},
};

const COMPACT_CONFIRM_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug)]
pub(super) struct CompactOptions {
    pub(super) model: String,
    pub(super) effort: String,
    pub(super) timeout: u64,
    pub(super) no_resume: bool,
    pub(super) verbose: bool,
}

#[derive(Clone, Debug)]
struct CompactSession {
    thread_id: String,
    path: PathBuf,
    marker_count: usize,
}

#[derive(Debug, Deserialize)]
struct AppServerMessage {
    id: Option<u64>,
    method: Option<String>,
    params: Option<Value>,
    error: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct CompactLogEnvelope {
    #[serde(rename = "type")]
    kind: String,
    payload: Value,
}

#[derive(Debug, Deserialize)]
struct CompactSessionMeta {
    id: String,
    cwd: String,
}

#[derive(Debug)]
enum AppServerEvent {
    Message(AppServerMessage),
    Stderr(String),
}

#[derive(Debug, Default)]
struct AppServerSignals {
    compacted: bool,
}

pub(super) fn run_compact(options: CompactOptions) -> Result<i32> {
    if options.timeout == 0 {
        bail!("`za codex compact --timeout` must be greater than zero");
    }

    let ctx = resolve_workspace_context()?;
    ensure_compact_can_run(&ctx, options.no_resume)?;

    let session = find_latest_compact_session(&ctx.workspace_root)?.ok_or_else(|| {
        anyhow!(
            "No Codex conversation found for `{}`. Start one with `za codex resume` first.",
            ctx.workspace_root.display()
        )
    })?;

    println!(
        "compact  {}  {}",
        ctx.workspace_label,
        truncate_end(&session.thread_id, 12)
    );
    println!("model    {} {}", options.model, options.effort);

    run_app_server_compaction(&ctx, &session, &options)?;
    println!("status   compacted");

    if options.no_resume {
        return Ok(0);
    }

    println!("resume   starting za codex resume");
    run_resume_with_args(&[], false, "compact")
}

fn ensure_compact_can_run(ctx: &WorkspaceContext, no_resume: bool) -> Result<()> {
    match probe_tmux()? {
        TmuxProbe::Available => {
            if tmux_has_session(&ctx.session_name)? {
                bail!(
                    "Codex session `{}` is still running. Exit it first, then run `za codex compact`.",
                    ctx.session_name
                );
            }
        }
        TmuxProbe::Missing if !no_resume => {
            bail!(
                "`za codex compact` needs `tmux` to resume afterwards; use `--no-resume` to compact only"
            )
        }
        TmuxProbe::Missing => {}
    }
    Ok(())
}

fn run_app_server_compaction(
    ctx: &WorkspaceContext,
    session: &CompactSession,
    options: &CompactOptions,
) -> Result<()> {
    let timeout = Duration::from_secs(options.timeout);
    let mut client = AppServerClient::start(&ctx.workspace_root, options.verbose)?;

    client.request("initialize", initialize_params(), timeout)?;
    client.request(
        "thread/resume",
        resume_params(ctx, session, options),
        timeout,
    )?;
    client.request(
        "thread/compact/start",
        json!({ "threadId": session.thread_id }),
        timeout,
    )?;

    let markers_after = client.wait_for_compaction(
        &session.thread_id,
        &session.path,
        session.marker_count,
        timeout,
    )?;

    if options.verbose {
        eprintln!(
            "compact markers {} -> {}",
            session.marker_count, markers_after
        );
    }
    Ok(())
}

fn initialize_params() -> Value {
    json!({
        "clientInfo": {
            "name": "za-codex-compact",
            "title": null,
            "version": env!("CARGO_PKG_VERSION"),
        },
        "capabilities": {
            "experimentalApi": true,
            "requestAttestation": false,
            "optOutNotificationMethods": [],
        },
    })
}

fn resume_params(
    ctx: &WorkspaceContext,
    session: &CompactSession,
    options: &CompactOptions,
) -> Value {
    json!({
        "threadId": session.thread_id,
        "cwd": ctx.workspace_root,
        "model": options.model,
        "config": {
            "model_reasoning_effort": options.effort,
        },
        "excludeTurns": true,
        "persistExtendedHistory": false,
    })
}

struct AppServerClient {
    child: Child,
    receiver: Receiver<AppServerEvent>,
    next_id: u64,
    verbose: bool,
}

impl AppServerClient {
    fn start(workspace_root: &Path, verbose: bool) -> Result<Self> {
        let codex = crate::command::run::resolve_executable_path("codex")?;
        let mut command = Command::new(&codex);
        command
            .args(["app-server", "--listen", "stdio://"])
            .current_dir(workspace_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        for (key, value) in crate::command::run::normalized_proxy_env_from_system()? {
            command.env(key, value);
        }
        for (key, value) in crate::command::ai::codex_env_overrides(workspace_root)? {
            command.env(key, value);
        }

        let mut child = command
            .spawn()
            .with_context(|| format!("start `{}` app-server", codex.display()))?;
        let stdout = child
            .stdout
            .take()
            .context("open codex app-server stdout")?;
        let stderr = child
            .stderr
            .take()
            .context("open codex app-server stderr")?;
        let (sender, receiver) = mpsc::channel();

        let stdout_sender = sender.clone();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(|line| line.ok()) {
                if let Ok(message) = serde_json::from_str::<AppServerMessage>(&line) {
                    let _ = stdout_sender.send(AppServerEvent::Message(message));
                }
            }
        });

        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(|line| line.ok()) {
                let _ = sender.send(AppServerEvent::Stderr(line));
            }
        });

        Ok(Self {
            child,
            receiver,
            next_id: 1,
            verbose,
        })
    }

    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<()> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        if self.verbose {
            eprintln!("> {method} id={id}");
        }
        let stdin = self
            .child
            .stdin
            .as_mut()
            .context("codex app-server stdin is closed")?;
        writeln!(stdin, "{request}").context("write codex app-server request")?;
        stdin.flush().context("flush codex app-server request")?;
        self.wait_for_response(id, timeout).map(|_| ())
    }

    fn wait_for_response(&mut self, id: u64, timeout: Duration) -> Result<AppServerSignals> {
        let deadline = Instant::now() + timeout;
        let mut signals = AppServerSignals::default();
        loop {
            if let Some(status) = self.child.try_wait().context("poll codex app-server")? {
                bail!("codex app-server exited before response id={id}: {status}");
            }

            let remaining = remaining_until(deadline)?;
            match self
                .receiver
                .recv_timeout(remaining.min(COMPACT_CONFIRM_POLL_INTERVAL))
            {
                Ok(event) => {
                    if self.handle_event(event, &mut signals)? == Some(id) {
                        return Ok(signals);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    bail!("codex app-server output closed before response id={id}");
                }
            }
        }
    }

    fn wait_for_compaction(
        &mut self,
        thread_id: &str,
        rollout_path: &Path,
        markers_before: usize,
        timeout: Duration,
    ) -> Result<usize> {
        let deadline = Instant::now() + timeout;
        let mut signals = AppServerSignals::default();
        loop {
            let markers_now = count_compaction_markers(rollout_path)?;
            if markers_now > markers_before {
                return Ok(markers_now);
            }
            if signals.compacted {
                return Ok(markers_now);
            }
            if let Some(status) = self.child.try_wait().context("poll codex app-server")? {
                bail!("codex app-server exited before compaction was confirmed: {status}");
            }

            let remaining = remaining_until(deadline)?;
            match self
                .receiver
                .recv_timeout(remaining.min(COMPACT_CONFIRM_POLL_INTERVAL))
            {
                Ok(event) => {
                    self.handle_event(event, &mut signals)?;
                    if signals.compacted {
                        if self.verbose {
                            eprintln!("< confirmed thread/compacted {thread_id}");
                        }
                        return count_compaction_markers(rollout_path);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    bail!("codex app-server output closed before compaction was confirmed");
                }
            }
        }
    }

    fn handle_event(
        &self,
        event: AppServerEvent,
        signals: &mut AppServerSignals,
    ) -> Result<Option<u64>> {
        match event {
            AppServerEvent::Stderr(line) => {
                if self.verbose {
                    eprintln!("stderr: {line}");
                }
                Ok(None)
            }
            AppServerEvent::Message(message) => {
                if let Some(method) = message.method.as_deref() {
                    if self.verbose {
                        eprintln!("< notification {method}");
                    }
                    match method {
                        "thread/compacted" => {
                            signals.compacted = true;
                        }
                        "error" => {
                            bail!("codex app-server error: {}", compact_json(&message.params));
                        }
                        _ => {}
                    }
                }
                if let Some(error) = message.error {
                    bail!(
                        "codex app-server request failed: {}",
                        compact_json(&Some(error))
                    );
                }
                Ok(message.id)
            }
        }
    }
}

impl Drop for AppServerClient {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn remaining_until(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| anyhow!("timed out waiting for codex app-server"))
}

fn compact_json(value: &Option<Value>) -> String {
    value
        .as_ref()
        .map(Value::to_string)
        .unwrap_or_else(|| "<empty>".to_string())
}

fn find_latest_compact_session(workspace_root: &Path) -> Result<Option<CompactSession>> {
    let workspace_root = canonical_path(workspace_root)?;
    let mut best: Option<CompactSession> = None;
    for path in discover_codex_session_paths()? {
        let Some(candidate) = scan_compact_session(&path)? else {
            continue;
        };
        let Ok(candidate_root) = canonical_path(Path::new(&candidate.workspace_root)) else {
            continue;
        };
        if candidate_root != workspace_root {
            continue;
        }
        let should_replace = best
            .as_ref()
            .is_none_or(|current| modified_unix(&candidate.path) > modified_unix(&current.path));
        if should_replace {
            best = Some(CompactSession {
                thread_id: candidate.thread_id,
                path: candidate.path,
                marker_count: candidate.marker_count,
            });
        }
    }
    Ok(best)
}

#[derive(Debug)]
struct CompactSessionScan {
    thread_id: String,
    workspace_root: String,
    path: PathBuf,
    marker_count: usize,
}

fn scan_compact_session(path: &Path) -> Result<Option<CompactSessionScan>> {
    let mut thread_id = None;
    let mut workspace_root = None;
    let mut marker_count = 0;

    read_lossy_jsonl_lines(path, |line| {
        if is_compaction_marker_line(line) {
            marker_count += 1;
        }
        let Ok(envelope) = serde_json::from_str::<CompactLogEnvelope>(line) else {
            return;
        };
        if envelope.kind != "session_meta" {
            return;
        }
        if let Ok(payload) = serde_json::from_value::<CompactSessionMeta>(envelope.payload) {
            thread_id = Some(payload.id);
            workspace_root = Some(payload.cwd);
        }
    })?;

    Ok(match (thread_id, workspace_root) {
        (Some(thread_id), Some(workspace_root)) => Some(CompactSessionScan {
            thread_id,
            workspace_root,
            path: path.to_path_buf(),
            marker_count,
        }),
        _ => None,
    })
}

fn count_compaction_markers(path: &Path) -> Result<usize> {
    let mut count = 0;
    read_lossy_jsonl_lines(path, |line| {
        if is_compaction_marker_line(line) {
            count += 1;
        }
    })?;
    Ok(count)
}

fn read_lossy_jsonl_lines(path: &Path, mut visit: impl FnMut(&str)) -> Result<()> {
    let file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut buffer = Vec::new();

    loop {
        buffer.clear();
        let bytes = reader
            .read_until(b'\n', &mut buffer)
            .with_context(|| format!("read {}", path.display()))?;
        if bytes == 0 {
            break;
        }
        while matches!(buffer.last(), Some(b'\n' | b'\r')) {
            buffer.pop();
        }
        let line = String::from_utf8_lossy(&buffer);
        visit(&line);
    }

    Ok(())
}

fn is_compaction_marker_line(line: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return false;
    };
    if value.get("method").and_then(Value::as_str) == Some("thread/compacted") {
        return true;
    }
    if value.get("type").and_then(Value::as_str) == Some("compacted") {
        return true;
    }
    if value.get("type").and_then(Value::as_str) == Some("response_item") {
        let payload_type = value
            .get("payload")
            .and_then(|payload| payload.get("type"))
            .and_then(Value::as_str);
        return matches!(payload_type, Some("contextCompaction" | "compacted"));
    }
    false
}

fn canonical_path(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path).with_context(|| format!("canonicalize {}", path.display()))
}

fn modified_unix(path: &Path) -> u64 {
    fs::metadata(path)
        .ok()
        .map(|metadata| metadata_modified_unix(&metadata))
        .unwrap_or_default()
}

fn metadata_modified_unix(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_marker_detection_covers_current_and_legacy_shapes() {
        assert!(is_compaction_marker_line(
            r#"{"type":"response_item","payload":{"type":"contextCompaction"}}"#
        ));
        assert!(is_compaction_marker_line(
            r#"{"method":"thread/compacted","params":{"threadId":"t"}}"#
        ));
        assert!(is_compaction_marker_line(r#"{"type":"compacted"}"#));
        assert!(!is_compaction_marker_line(
            r#"{"type":"message","payload":{"content":"contextCompaction"}}"#
        ));
        assert!(!is_compaction_marker_line(r#"{"type":"message"}"#));
    }

    #[test]
    fn initialize_params_match_app_server_schema() {
        let params = initialize_params();
        assert_eq!(params["clientInfo"]["name"], "za-codex-compact");
        assert_eq!(params["capabilities"]["experimentalApi"], true);
        assert_eq!(params["capabilities"]["requestAttestation"], false);
    }

    #[test]
    fn compact_session_scan_tolerates_non_utf8_rollout_lines() {
        let path = temp_rollout_path();
        let mut content = Vec::new();
        content.extend_from_slice(
            br#"{"type":"response_item","payload":{"type":"contextCompaction"}}"#,
        );
        content.push(b'\n');
        content.extend_from_slice(b"\xff\xfe invalid utf8\n");
        content.extend_from_slice(
            br#"{"type":"session_meta","payload":{"id":"019e052b-a72f-7ef3-af56-2ce01bc9230f","cwd":"/tmp/work"}}"#,
        );
        content.push(b'\n');
        content.extend_from_slice(br#"{"method":"thread/compacted","params":{"threadId":"t"}}"#);
        content.push(b'\n');
        fs::write(&path, content).expect("write rollout");

        let scan = scan_compact_session(&path)
            .expect("scan rollout")
            .expect("session metadata");
        assert_eq!(scan.thread_id, "019e052b-a72f-7ef3-af56-2ce01bc9230f");
        assert_eq!(scan.workspace_root, "/tmp/work");
        assert_eq!(scan.marker_count, 2);
        assert_eq!(count_compaction_markers(&path).expect("count markers"), 2);

        let _ = fs::remove_file(path);
    }

    fn temp_rollout_path() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "za-compact-rollout-{}-{nanos}.jsonl",
            std::process::id()
        ))
    }
}
