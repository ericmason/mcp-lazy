//! The few system calls the proxy needs that std doesn't cover.

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("mcp-lazy supports only macOS and Linux");

use std::ffi::{CStr, OsStr};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::IntoRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};

pub const SIGTERM: i32 = libc::SIGTERM;
pub const SIGKILL: i32 = libc::SIGKILL;
/// Signals that end mcp-lazy, after it stops the server.
pub const STOP_SIGNALS: [i32; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];
pub const O_NOFOLLOW: i32 = libc::O_NOFOLLOW;
pub const O_NONBLOCK: i32 = libc::O_NONBLOCK;

/// Signals every process in the group led by `pgid`. Refuses pids that would
/// turn into a broadcast (0, 1, or anything that overflows i32).
pub fn kill_group(pgid: u32, sig: i32) {
    if pgid <= 1 || pgid > i32::MAX as u32 {
        return;
    }
    unsafe {
        libc::kill(-(pgid as i32), sig);
    }
}

/// True once `pid` has exited, without reaping it. While the zombie is
/// unreaped its pid, and so its process group id, can't be reused, which
/// keeps a later `kill_group` from hitting an unrelated group.
pub fn has_exited(pid: u32) -> bool {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, libc::WEXITED | libc::WNOHANG | libc::WNOWAIT)
    };
    if rc != 0 {
        // ECHILD: already reaped or not ours. Either way, nothing to wait for.
        return true;
    }
    unsafe { info.si_pid() != 0 }
}

/// Blocks until `pid` exits, without reaping it. Returns at once if it was
/// already reaped.
pub fn wait_exit(pid: u32) {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, libc::WEXITED | libc::WNOWAIT);
    }
}

pub fn euid() -> u32 {
    unsafe { libc::geteuid() }
}

/// This user's home directory from the user database, or None.
pub fn home_dir() -> Option<PathBuf> {
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    let mut buf = vec![0u8; 4096];
    loop {
        let rc = unsafe { libc::getpwuid_r(euid(), &mut pw, buf.as_mut_ptr().cast(), buf.len(), &mut out) };
        if rc == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 || out.is_null() || pw.pw_dir.is_null() {
            return None;
        }
        let dir = unsafe { CStr::from_ptr(pw.pw_dir) }.to_bytes();
        return (!dir.is_empty()).then(|| PathBuf::from(OsStr::from_bytes(dir)));
    }
}

/// Puts SIGCHLD back to the default. A parent that ignores SIGCHLD passes
/// that on, and then the kernel reaps the server by itself, freeing its pid
/// for reuse while `kill_group` might still signal it.
pub fn reset_sigchld() {
    unsafe {
        libc::signal(libc::SIGCHLD, libc::SIG_DFL);
    }
}

static SIGNAL_FD: AtomicI32 = AtomicI32::new(-1);

#[cfg(target_os = "macos")]
fn errno_ptr() -> *mut libc::c_int {
    unsafe { libc::__error() }
}

#[cfg(target_os = "linux")]
fn errno_ptr() -> *mut libc::c_int {
    unsafe { libc::__errno_location() }
}

extern "C" fn on_signal(sig: libc::c_int) {
    // The interrupted code may be about to read errno, so put it back.
    let errno = errno_ptr();
    let saved = unsafe { *errno };
    let fd = SIGNAL_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        let b = sig as u8;
        // write(2) is async-signal-safe. If the socket is full, a signal is
        // already waiting to be read, which is enough.
        unsafe {
            libc::write(fd, (&b as *const u8).cast(), 1);
        }
    }
    unsafe { *errno = saved };
}

/// Catches `STOP_SIGNALS` and returns the socket that each caught signal's
/// number is written to, one byte per signal. A signal that was already
/// ignored when mcp-lazy started, such as SIGHUP under `nohup` or SIGINT in a
/// background job, stays ignored.
pub fn catch_stop_signals() -> std::io::Result<UnixStream> {
    let (rx, tx) = UnixStream::pair()?;
    tx.set_nonblocking(true)?;
    // The write end stays open for the life of the process.
    SIGNAL_FD.store(tx.into_raw_fd(), Ordering::Relaxed);
    let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    for sig in STOP_SIGNALS {
        let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
        if unsafe { libc::sigaction(sig, std::ptr::null(), &mut old) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if old.sa_sigaction == libc::SIG_IGN {
            continue;
        }
        if unsafe { libc::signal(sig, handler) } == libc::SIG_ERR {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(rx)
}

/// Writes a line to stderr and ignores errors, so a closed stderr can't stop
/// mcp-lazy. Use it through `note!`.
pub fn note(args: std::fmt::Arguments) {
    let mut e = std::io::stderr().lock();
    let _ = e.write_fmt(args);
    let _ = e.write_all(b"\n");
}

/// Writes a line to stdout and ignores errors. Use it through `say!`.
pub fn say(args: std::fmt::Arguments) {
    let mut o = std::io::stdout().lock();
    let _ = o.write_fmt(args);
    let _ = o.write_all(b"\n");
    let _ = o.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::{Duration, Instant};

    #[test]
    fn sees_an_exit_without_reaping() {
        let mut c = Command::new("true").spawn().unwrap();
        let pid = c.id();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !has_exited(pid) {
            assert!(Instant::now() < deadline, "child never seen as exited");
            std::thread::sleep(Duration::from_millis(10));
        }
        // Still unreaped: the real wait gets its status.
        assert!(c.wait().unwrap().success());
    }

    #[test]
    fn wait_exit_returns_on_exit_and_leaves_the_status() {
        let mut c = Command::new("sleep").arg("0.2").spawn().unwrap();
        let t = Instant::now();
        wait_exit(c.id());
        assert!(t.elapsed() >= Duration::from_millis(150));
        assert!(c.wait().unwrap().success());
    }

    #[test]
    fn a_running_child_has_not_exited() {
        let mut c = Command::new("sleep").arg("5").spawn().unwrap();
        assert!(!has_exited(c.id()));
        c.kill().unwrap();
        c.wait().unwrap();
    }

    #[test]
    fn finds_a_home_directory() {
        assert!(home_dir().is_some_and(|d| d.is_absolute()));
    }

    #[test]
    fn the_signal_handler_leaves_errno_alone() {
        // A write to a closed descriptor fails and sets errno to EBADF.
        SIGNAL_FD.store(999_999, Ordering::Relaxed);
        unsafe { *errno_ptr() = libc::EINTR };
        on_signal(libc::SIGTERM);
        let after = unsafe { *errno_ptr() };
        SIGNAL_FD.store(-1, Ordering::Relaxed);
        assert_eq!(after, libc::EINTR);
    }
}
