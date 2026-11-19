#![cfg(unix)]

use rustix::process::{Pid, Signal, kill_process};
use std::{
    fs, io,
    net::TcpListener,
    process::{Child, Command, Output, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

fn interrupt_stalled_request(
    mut cmd: Command,
    interrupt: impl FnOnce(&Child) -> io::Result<()>,
) -> Output {
    let proxy = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let addr = proxy.local_addr().unwrap();
    let (connected, accepted) = mpsc::channel();
    thread::spawn(move || {
        let (socket, _) = proxy.accept().unwrap();
        let _ = connected.send((proxy, socket));
    });

    let home = std::env::temp_dir().join(format!(
        "za-test-interrupt-{}-{}",
        std::process::id(),
        addr.port()
    ));
    fs::create_dir(&home).unwrap();
    fs::write(
        home.join("Cargo.toml"),
        "[package]\nname = 'fixture'\nversion = '0.1.0'\n[dependencies]\nserde = '1'\n",
    )
    .unwrap();
    let current = home.join(".local/state/za/tools/current");
    fs::create_dir_all(&current).unwrap();
    fs::write(current.join("fd"), "1.0.0").unwrap();
    fs::write(home.join("za.tools.toml"), "tools = ['fd', 'just']\n").unwrap();
    let mut child = cmd
        .current_dir(&home)
        .env_clear()
        .env("HOME", &home)
        .env("TMPDIR", &home)
        .env("HTTPS_PROXY", format!("http://{addr}"))
        .spawn()
        .unwrap();

    // Keep CONNECT unanswered until the CLI exits. Set a deadline even on
    // failure so a regression cannot leave a process waiting in the test suite.
    let result = (|| -> io::Result<()> {
        let _proxy_and_socket = accepted
            .recv_timeout(Duration::from_secs(5))
            .map_err(io::Error::other)?;
        interrupt(&child)?;
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
    output
}

#[test]
fn interrupt_update_while_github_is_unresponsive() {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_za"));
    cmd.args(["tool", "--user", "update", "fd", "just"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = interrupt_stalled_request(cmd, |child| {
        Ok(kill_process(Pid::from_child(child), Signal::INT)?)
    });
    assert_eq!(output.status.code(), Some(130));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("fail"));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "Interrupted"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn progress_commands_silence_keys_and_restore_echo_after_ctrl_c() {
    for args in [
        &["tool", "--user", "update", "fd", "just"][..],
        &["tool", "--user", "install", "fd"],
        &["tool", "--user", "sync", "--file", "za.tools.toml"],
        &["tool", "--user", "ls", "--outdated", "fd"],
        &["update", "--user", "--check"],
        &["update", "--user"],
        &["deps"],
        &["deps", "audit"],
    ] {
        check_terminal_interrupt(args);
    }
}

#[cfg(target_os = "linux")]
fn check_terminal_interrupt(args: &[&str]) {
    use rustix::{
        io::ioctl_fionread,
        process::{ioctl_tiocsctty, setsid},
        pty::{OpenptFlags, grantpt, ioctl_tiocgptpeer, openpt, unlockpt},
        termios::{LocalModes, OptionalActions, tcgetattr, tcsetattr},
    };
    use std::{
        io::Write,
        os::unix::process::{CommandExt, ExitStatusExt},
    };

    let flags = OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC;
    let mut master = fs::File::from(openpt(flags).unwrap());
    grantpt(&master).unwrap();
    unlockpt(&master).unwrap();
    let slave = ioctl_tiocgptpeer(&master, flags).unwrap();
    let echo = LocalModes::ECHO | LocalModes::ECHONL;
    let mut original = tcgetattr(&slave).unwrap();
    // NOFLSH makes pending input observable even after the kernel delivers SIGINT.
    original.local_modes.insert(echo | LocalModes::NOFLSH);
    tcsetattr(&slave, OptionalActions::Now, &original).unwrap();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_za"));
    cmd.args(args)
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave.try_clone().unwrap());
    let tty = slave.try_clone().unwrap();
    // SAFETY: the post-fork hook only performs the setsid and ioctl syscalls.
    unsafe {
        cmd.pre_exec(move || {
            setsid()?;
            Ok(ioctl_tiocsctty(&tty)?)
        });
    }

    let output = interrupt_stalled_request(cmd, |_| {
        if tcgetattr(&slave)?.local_modes != original.local_modes - echo {
            return Err(io::Error::other(format!(
                "{args:?} should suppress only echo"
            )));
        }
        master.write_all(b"\x1b[D\x1b[D\n\x03")
    });
    if args[0] == "deps" {
        assert_eq!(
            output.status.signal(),
            Some(signal_hook::consts::SIGINT),
            "{args:?}"
        );
    } else {
        assert_eq!(output.status.code(), Some(130), "{args:?}");
    }
    assert_eq!(tcgetattr(&slave).unwrap().local_modes, original.local_modes);
    assert_eq!(ioctl_fionread(&slave).unwrap(), 0);
}
