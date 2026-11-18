//! A thread that owns one output stream (the server's stdin or our stdout),
//! so the event loop never blocks on a reader that stopped reading.

use std::io::{self, Write};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread;
use std::time::Duration;

/// Lines a writer holds while its reader isn't keeping up. One more line can
/// be in the middle of a write, and the pipe holds some more.
pub const QUEUE_LINES: usize = 1024;

#[derive(Debug, PartialEq)]
pub enum SendError {
    /// `QUEUE_LINES` lines are already waiting.
    Full,
    /// A write failed, so the writer stopped.
    Closed,
}

pub struct Writer {
    tx: Option<SyncSender<Vec<u8>>>,
    /// Disconnects when the thread ends, after it has written everything queued.
    done: Receiver<()>,
}

impl Writer {
    /// Starts the thread. `on_error` runs once, on the thread, if a write fails.
    pub fn spawn<W: Write + Send + 'static>(
        name: &str,
        mut out: W,
        on_error: impl FnOnce() + Send + 'static,
    ) -> io::Result<Writer> {
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(QUEUE_LINES);
        let (done_tx, done) = mpsc::channel::<()>();
        thread::Builder::new().name(name.into()).stack_size(64 * 1024).spawn(move || {
            let _done = done_tx;
            // Ends when the sender is dropped and the queue is empty, which
            // closes `out`, or when a write fails.
            for line in rx {
                if out.write_all(&line).and_then(|_| out.flush()).is_err() {
                    on_error();
                    return;
                }
            }
        })?;
        Ok(Writer { tx: Some(tx), done })
    }

    /// Queues `line` and a newline without blocking.
    pub fn send(&self, line: &[u8]) -> Result<(), SendError> {
        let tx = self.tx.as_ref().ok_or(SendError::Closed)?;
        let mut v = Vec::with_capacity(line.len() + 1);
        v.extend_from_slice(line);
        v.push(b'\n');
        tx.try_send(v).map_err(|e| match e {
            TrySendError::Full(_) => SendError::Full,
            TrySendError::Disconnected(_) => SendError::Closed,
        })
    }

    /// Stops taking lines and waits up to `timeout` for the queued ones to be
    /// written. Returns false if they weren't all written in time.
    pub fn finish(mut self, timeout: Duration) -> bool {
        self.tx.take();
        matches!(self.done.recv_timeout(timeout), Err(mpsc::RecvTimeoutError::Disconnected))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use std::io::Read;

    #[test]
    fn fills_up_instead_of_blocking_and_drains_in_order() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let w = Writer::spawn("t", a, || {}).unwrap();
        let line = vec![b'x'; 64 * 1024];
        // Nobody reads `b`, so the queue fills and send says so.
        let mut sent = 0;
        while w.send(&line) == Ok(()) {
            sent += 1;
            assert!(sent <= QUEUE_LINES + 64, "send never reported a full queue");
        }
        assert_eq!(w.send(&line), Err(SendError::Full));
        let reader = thread::spawn(move || {
            let mut all = Vec::new();
            b.read_to_end(&mut all).unwrap();
            all.len()
        });
        assert!(w.finish(Duration::from_secs(10)));
        assert_eq!(reader.join().unwrap(), sent * (line.len() + 1));
    }

    #[test]
    fn a_failed_write_closes_the_writer() {
        let (a, b) = UnixStream::pair().unwrap();
        drop(b);
        let (tx, rx) = mpsc::channel();
        let w = Writer::spawn("t", a, move || tx.send(()).unwrap()).unwrap();
        let _ = w.send(b"x");
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while w.send(b"x") != Err(SendError::Closed) {
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
    }
}
