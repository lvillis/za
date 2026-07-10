//! Bounded child-process execution and Linux process identity helpers.

use anyhow::{Context, Result, bail};
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

const POLL_INTERVAL: Duration = Duration::from_millis(20);
const MAX_CAPTURE_BYTES: u64 = 1024 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct ProcessIdentity {
    pub pid: i32,
    pub start_ticks: u64,
}

pub fn output_with_timeout(command: &mut Command, timeout: Duration) -> Result<Output> {
    let label = format!("{:?}", command);
    let stdout = TempCapture::create("stdout")?;
    let stderr = TempCapture::create("stderr")?;
    command
        .stdout(Stdio::from(stdout.writer()?))
        .stderr(Stdio::from(stderr.writer()?));
    let mut child = command
        .spawn()
        .with_context(|| format!("start command {label}"))?;
    let started = Instant::now();

    let status = loop {
        if stdout.exceeds_limit()? || stderr.exceeds_limit()? {
            let _ = child.kill();
            let _ = child.wait();
            bail!("command output exceeded {MAX_CAPTURE_BYTES} bytes per stream: {label}");
        }
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("wait for command {label}"))?
        {
            break status;
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "command timed out after {:.1}s: {label}",
                timeout.as_secs_f64()
            );
        }
        thread::sleep(POLL_INTERVAL);
    };
    if stdout.exceeds_limit()? || stderr.exceeds_limit()? {
        bail!("command output exceeded {MAX_CAPTURE_BYTES} bytes per stream: {label}");
    }

    Ok(Output {
        status,
        stdout: stdout.read_limited()?,
        stderr: stderr.read_limited()?,
    })
}

pub fn capture_process_identity(pid: u32) -> Option<ProcessIdentity> {
    let pid = i32::try_from(pid).ok()?;
    read_process_start_ticks(pid).map(|start_ticks| ProcessIdentity { pid, start_ticks })
}

pub fn process_identity_matches(identity: ProcessIdentity) -> bool {
    read_process_start_ticks(identity.pid) == Some(identity.start_ticks)
}

#[cfg(target_os = "linux")]
fn read_process_start_ticks(pid: i32) -> Option<u64> {
    if pid <= 0 {
        return None;
    }
    let raw = fs::read_to_string(Path::new("/proc").join(pid.to_string()).join("stat")).ok()?;
    parse_linux_proc_start_ticks(&raw)
}

#[cfg(not(target_os = "linux"))]
fn read_process_start_ticks(_pid: i32) -> Option<u64> {
    None
}

fn parse_linux_proc_start_ticks(raw: &str) -> Option<u64> {
    let fields = raw.get(raw.rfind(") ")? + 2..)?;
    fields.split_whitespace().nth(19)?.parse().ok()
}

struct TempCapture {
    path: PathBuf,
}

impl TempCapture {
    fn create(stream: &str) -> Result<Self> {
        let temp_dir = std::env::temp_dir();
        for _ in 0..100 {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = temp_dir.join(format!(
                "za-command-{}-{sequence}-{stream}.tmp",
                std::process::id()
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Ok(Self { path }),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    return Err(err).with_context(|| format!("create {}", path.display()));
                }
            }
        }
        bail!("cannot allocate temporary command output file")
    }

    fn writer(&self) -> Result<File> {
        OpenOptions::new()
            .append(true)
            .open(&self.path)
            .with_context(|| format!("open {}", self.path.display()))
    }

    fn read_limited(&self) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        File::open(&self.path)
            .with_context(|| format!("open {}", self.path.display()))?
            .take(MAX_CAPTURE_BYTES)
            .read_to_end(&mut bytes)
            .with_context(|| format!("read {}", self.path.display()))?;
        Ok(bytes)
    }

    fn exceeds_limit(&self) -> Result<bool> {
        Ok(fs::metadata(&self.path)
            .with_context(|| format!("inspect {}", self.path.display()))?
            .len()
            > MAX_CAPTURE_BYTES)
    }
}

impl Drop for TempCapture {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_start_ticks_parser_handles_spaces_and_parentheses_in_comm() {
        let mut fields = vec!["S"; 20];
        fields[19] = "4242";
        let raw = format!("123 (worker ) name) {}", fields.join(" "));
        assert_eq!(parse_linux_proc_start_ticks(&raw), Some(4242));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_command_execution_times_out() {
        let error = output_with_timeout(
            Command::new("sh").args(["-c", "sleep 2"]),
            Duration::from_millis(50),
        )
        .expect_err("command must time out");
        assert!(error.to_string().contains("timed out"));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_command_execution_rejects_excessive_output() {
        let error = output_with_timeout(
            Command::new("sh").args(["-c", "yes x | head -c 1100000"]),
            Duration::from_secs(2),
        )
        .expect_err("oversized output must fail");
        assert!(error.to_string().contains("output exceeded"));
    }
}
