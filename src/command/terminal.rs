use rustix::{
    io::retry_on_intr,
    process::getpgrp,
    termios::{LocalModes, OptionalActions, Termios, tcgetattr, tcgetpgrp, tcsetattr},
};
use signal_hook::{
    consts::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM},
    iterator::{Handle, Signals},
    low_level::emulate_default_handler,
};
use std::{io, os::fd::AsFd, thread};

const ECHO: LocalModes = LocalModes::ECHO.union(LocalModes::ECHONL);

fn foreground_echo(output: impl AsFd) -> Option<Termios> {
    let foreground = Ok(getpgrp());
    if tcgetpgrp(io::stdin()) != foreground || tcgetpgrp(output) != foreground {
        return None;
    }
    let attrs = retry_on_intr(|| tcgetattr(io::stdin())).ok()?;
    attrs.local_modes.intersects(ECHO).then_some(attrs)
}

struct TerminalEcho {
    saved_echo: LocalModes,
}

impl TerminalEcho {
    fn disable(mut attrs: Termios) -> io::Result<Self> {
        let saved_echo = attrs.local_modes & ECHO;
        attrs.local_modes.remove(ECHO);
        retry_on_intr(|| tcsetattr(io::stdin(), OptionalActions::Now, &attrs))?;
        Ok(Self { saved_echo })
    }
}

impl Drop for TerminalEcho {
    fn drop(&mut self) {
        let stdin = io::stdin();
        if tcgetpgrp(&stdin) == Ok(getpgrp())
            && let Ok(mut attrs) = retry_on_intr(|| tcgetattr(&stdin))
        {
            attrs.local_modes.insert(self.saved_echo);
            // Discard keys typed during progress before returning the terminal to the shell.
            let _ = retry_on_intr(|| tcsetattr(&stdin, OptionalActions::Flush, &attrs));
        }
    }
}

/// Restores echo before returning to the shell, including on terminating signals.
pub(super) struct ProgressTerminal {
    signals: Handle,
    listener: Option<thread::JoinHandle<()>>,
}

impl ProgressTerminal {
    pub(super) fn acquire(output: impl AsFd) -> io::Result<Option<Self>> {
        Self::with_signals(output, &[SIGINT, SIGTERM, SIGHUP, SIGQUIT])
    }

    /// The tool manager already unwinds its downloads and locks on SIGINT and SIGTERM.
    pub(super) fn for_tool_progress() -> io::Result<Option<Self>> {
        Self::with_signals(io::stderr(), &[SIGHUP, SIGQUIT])
    }

    fn with_signals(output: impl AsFd, terminating_signals: &[i32]) -> io::Result<Option<Self>> {
        let Some(attrs) = foreground_echo(output) else {
            return Ok(None);
        };
        // Register before changing the terminal. A signal arriving during setup stays pending.
        let mut signals = Signals::new(terminating_signals)?;
        let handle = signals.handle();
        let echo = TerminalEcho::disable(attrs)?;
        let listener = thread::Builder::new().spawn(move || {
            let signal = signals
                .forever()
                .next()
                .or_else(|| signals.pending().next());
            drop(echo);
            if let Some(signal) = signal {
                let _ = emulate_default_handler(signal);
            }
        })?;
        Ok(Some(Self {
            signals: handle,
            listener: Some(listener),
        }))
    }
}

impl Drop for ProgressTerminal {
    fn drop(&mut self) {
        self.signals.close();
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
    }
}
