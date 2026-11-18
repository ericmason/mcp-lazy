//! The real MCP server process.

use crate::proxy::Event;
use crate::sys;
use crate::writer::{SendError, Writer};
use std::io::{self, BufRead, BufReader, Read};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

/// The longest line, without its line ending, that mcp-lazy passes on in
/// either direction.
pub const MAX_LINE: usize = 64 << 20;

pub struct Child {
    proc: std::process::Child,
    /// The server's stdin. A thread of its own does the writing, so a server
    /// that stops reading can't block the event loop.
    stdin: Option<Writer>,
}

impl Child {
    /// Starts the server in its own process group, so stopping it also stops
    /// whatever it launched (npx, uvx, and shell wrappers leave children).
    /// Its stdout lines arrive as `Event::Server(gen, line)`; stderr passes
    /// straight through to ours. `Event::ServerExit(gen)` arrives when the
    /// process exits, even if something it started still holds stdout open,
    /// and `Event::ServerInputClosed(gen)` when a write to its stdin fails.
    pub fn spawn(argv: &[String], gen: u64, tx: Sender<Event>) -> io::Result<Child> {
        let mut proc = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .process_group(0)
            .spawn()?;
        let stdout = proc.stdout.take().expect("piped stdout");
        let stdin = proc.stdin.take().expect("piped stdin");
        let pid = proc.id();
        let exit_tx = tx.clone();
        let input_tx = tx.clone();
        let mut writer = None;
        let spawned = thread::Builder::new()
            .name("server-stdout".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                read_lines(stdout, MAX_LINE, |l| match l {
                    Line::Data(l) => tx.send(Event::Server(gen, l)).is_ok(),
                    Line::TooLong => {
                        note!("mcp-lazy: dropped a line from the MCP server longer than {} MiB", MAX_LINE >> 20);
                        true
                    }
                });
                let _ = tx.send(Event::ServerEof(gen));
            })
            .and_then(|_| {
                thread::Builder::new().name("server-exit".into()).stack_size(64 * 1024).spawn(move || {
                    sys::wait_exit(pid);
                    let _ = exit_tx.send(Event::ServerExit(gen));
                })
            })
            .and_then(|_| {
                writer = Some(Writer::spawn("server-stdin", stdin, move || {
                    let _ = input_tx.send(Event::ServerInputClosed(gen));
                })?);
                Ok(())
            });
        match (spawned, writer) {
            (Ok(()), Some(w)) => Ok(Child { proc, stdin: Some(w) }),
            (r, _) => {
                let mut c = Child { proc, stdin: None };
                c.kill_now();
                Err(r.err().unwrap_or_else(|| io::Error::other("could not start the stdin writer")))
            }
        }
    }

    /// Queues a line for the server's stdin without blocking.
    pub fn write(&mut self, line: &[u8]) -> Result<(), SendError> {
        self.stdin.as_ref().ok_or(SendError::Closed)?.send(line)
    }

    /// Closes stdin, sends SIGTERM to the group, waits up to 2 seconds, then
    /// SIGKILLs the group to catch anything still running. The leader is
    /// reaped only after the SIGKILL, so its group id can't have been reused.
    pub fn stop(mut self) {
        self.stdin.take();
        let pid = self.proc.id();
        sys::kill_group(pid, sys::SIGTERM);
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && !sys::has_exited(pid) {
            thread::sleep(Duration::from_millis(20));
        }
        sys::kill_group(pid, sys::SIGKILL);
        let _ = self.proc.wait();
    }

    fn kill_now(&mut self) {
        sys::kill_group(self.proc.id(), sys::SIGKILL);
        let _ = self.proc.wait();
    }
}

pub enum Line {
    /// A non-empty line without its line ending.
    Data(Vec<u8>),
    /// A line longer than the limit, already skipped.
    TooLong,
}

/// Calls `f` with each non-empty line until EOF, a read error, or `f` returns
/// false. A line longer than `max` bytes, not counting its `\r\n` or `\n`, is
/// skipped up to its newline and reported as `Line::TooLong`, so a line never
/// takes more than `max + 1` bytes of memory (room for its `\r`) plus the read
/// buffer, and the lines after it still arrive.
pub fn read_lines<R: Read>(r: R, max: usize, mut f: impl FnMut(Line) -> bool) {
    let mut br = BufReader::with_capacity(64 * 1024, r);
    let limit = max.saturating_add(1);
    let mut buf = Vec::new();
    let mut too_long = false;
    loop {
        let (used, ended) = match br.fill_buf() {
            Ok([]) => break,
            Ok(chunk) => {
                let end = chunk.iter().position(|&b| b == b'\n');
                let part = &chunk[..end.unwrap_or(chunk.len())];
                if !too_long {
                    let need = buf.len() + part.len();
                    // Grow by doubling, but never past `limit`, so the
                    // buffer's capacity stays within the cap.
                    let room = need <= limit
                        && (need <= buf.capacity()
                            || buf.try_reserve_exact(need.max(buf.capacity() * 2).min(limit) - buf.len()).is_ok());
                    if room {
                        buf.extend_from_slice(part);
                    } else {
                        too_long = true;
                        buf = Vec::new();
                    }
                }
                (end.map_or(chunk.len(), |i| i + 1), end.is_some())
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        br.consume(used);
        if ended && !deliver(&mut buf, max, &mut too_long, &mut f) {
            return;
        }
    }
    // A last line with no newline.
    deliver(&mut buf, max, &mut too_long, &mut f);
}

fn deliver(buf: &mut Vec<u8>, max: usize, too_long: &mut bool, f: &mut impl FnMut(Line) -> bool) -> bool {
    while buf.last() == Some(&b'\r') {
        buf.pop();
    }
    if std::mem::take(too_long) || buf.len() > max {
        *buf = Vec::new();
        return f(Line::TooLong);
    }
    buf.is_empty() || f(Line::Data(std::mem::take(buf)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(input: &[u8], max: usize) -> Vec<Option<String>> {
        let mut out = Vec::new();
        read_lines(input, max, |l| {
            out.push(match l {
                Line::Data(d) => Some(String::from_utf8(d).unwrap()),
                Line::TooLong => None,
            });
            true
        });
        out
    }

    #[test]
    fn splits_lines_and_skips_long_ones() {
        assert_eq!(lines(b"a\r\n\nbb\nccc", 3), [Some("a".into()), Some("bb".into()), Some("ccc".into())]);
        assert_eq!(lines(b"abcd\nok\nabcdef", 3), [None, Some("ok".into()), None]);
        // A line exactly at the limit passes.
        assert_eq!(lines(b"abc\n", 3), [Some("abc".into())]);
        // Its \r\n doesn't count against the limit.
        assert_eq!(lines(b"abc\r\n", 3), [Some("abc".into())]);
        assert_eq!(lines(b"abcd\r\n", 3), [None]);
        // Long lines that span many reads.
        let big = [vec![b'x'; 200_000], b"\nend\n".to_vec()].concat();
        assert_eq!(lines(&big, 100_000), [None, Some("end".into())]);
    }

    /// Hands out at most 1000 bytes per read, so a line arrives in pieces.
    struct Trickle<'a>(&'a [u8]);

    impl Read for Trickle<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let n = self.0.len().min(out.len()).min(1000);
            out[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }

    #[test]
    fn a_line_never_holds_more_memory_than_the_cap() {
        let input = [vec![b'x'; 5000], b"\n".to_vec()].concat();
        let mut cap = 0;
        read_lines(Trickle(&input), 5000, |l| {
            if let Line::Data(d) = l {
                assert_eq!(d.len(), 5000);
                cap = d.capacity();
            }
            true
        });
        assert!((5000..=5001).contains(&cap), "capacity {cap}");
    }
}
