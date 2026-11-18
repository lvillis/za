//! Keep blocking socket I/O off threads that own tool files, locks, and progress bars.
use super::*;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};

fn receive<T>(rx: &Receiver<T>) -> Result<T> {
    loop {
        ensure_not_interrupted()?;
        match rx.recv_timeout(INTERRUPT_POLL_INTERVAL) {
            Ok(value) => return Ok(value),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => bail!("HTTP worker stopped unexpectedly"),
        }
    }
}

pub(super) fn wait_for<T: Send + 'static>(
    operation: impl FnOnce() -> reqx::Result<T> + Send + 'static,
) -> Result<T> {
    ensure_not_interrupted()?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(operation());
    });
    Ok(receive(&rx)??)
}

// Only the socket reader may still be blocked when the command unwinds.
// The bounded channel limits read-ahead and dropping it stops the producer.
pub(super) struct ResponseChunks(Receiver<reqx::Result<Vec<u8>>>);

impl ResponseChunks {
    pub(super) fn next_chunk(&self) -> Result<Option<Vec<u8>>> {
        let bytes = receive(&self.0)??;
        Ok((!bytes.is_empty()).then_some(bytes))
    }
}

pub(super) fn chunks(
    mut response: reqx::blocking::ResponseStream,
    chunk_size: usize,
) -> ResponseChunks {
    let (tx, rx) = mpsc::sync_channel(1);
    thread::spawn(move || {
        loop {
            let mut buf = vec![0; chunk_size];
            let result = response.read_chunk(&mut buf).map(|n| {
                buf.truncate(n);
                buf
            });
            let done = !matches!(&result, Ok(bytes) if !bytes.is_empty());
            if tx.send(result).is_err() || done {
                break;
            }
        }
    });
    ResponseChunks(rx)
}
