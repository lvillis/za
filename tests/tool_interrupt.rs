#![cfg(unix)]

use rustix::process::{Pid, Signal, kill_process};
use std::{
    fs, io,
    net::TcpListener,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

#[test]
fn interrupt_update_while_github_is_unresponsive() {
    let proxy = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let addr = proxy.local_addr().unwrap();
    let (connected, accepted) = mpsc::channel();
    thread::spawn(move || {
        let (socket, _) = proxy.accept().unwrap();
        let _ = connected.send((proxy, socket));
    });

    let home = std::env::temp_dir().join(format!("za-test-interrupt-{}", std::process::id()));
    fs::create_dir(&home).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_za"))
        .args(["tool", "--user", "update", "fd", "just"])
        .env_clear()
        .env("HOME", &home)
        .env("TMPDIR", &home)
        .env("HTTPS_PROXY", format!("http://{addr}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Keep CONNECT unanswered until the CLI exits. Set a deadline even on
    // failure so a regression cannot leave a process waiting in the test suite.
    let result = (|| -> io::Result<()> {
        let _proxy_and_socket = accepted
            .recv_timeout(Duration::from_secs(5))
            .map_err(io::Error::other)?;
        kill_process(Pid::from_child(&child), Signal::INT)?;
        let deadline = Instant::now() + Duration::from_secs(2);
        while child.try_wait()?.is_none() {
            if Instant::now() >= deadline {
                return Err(io::Error::other(
                    "SIGINT did not cancel the blocked request",
                ));
            }
            thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    let output = child.wait_with_output().unwrap();
    fs::remove_dir_all(home).unwrap();
    result.unwrap();
    assert_eq!(output.status.code(), Some(130));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "Interrupted"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("fail"));
}
