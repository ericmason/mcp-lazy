/// Like `eprintln!`, but a closed or broken stderr is ignored instead of
/// panicking.
macro_rules! note {
    ($($t:tt)*) => { $crate::sys::note(format_args!($($t)*)) };
}

/// Like `println!`, but a closed or broken stdout is ignored.
macro_rules! say {
    ($($t:tt)*) => { $crate::sys::say(format_args!($($t)*)) };
}

mod cache;
mod child;
mod msg;
mod proxy;
mod sys;
mod writer;

use std::path::PathBuf;
use std::time::Duration;

const USAGE: &str = "\
Usage:
  mcp-lazy [options] -- <server command> [args...]
  mcp-lazy clear-cache [--cache-dir DIR]

Options:
  --idle SECONDS           stop the server after this long with nothing in flight (default 600, 0 = never)
  --discovery-idle SECONDS stop a server that has only answered discovery after this long (default 10, 0 = use --idle)
  --max-age SECONDS        ignore cached discovery replies older than this (default 604800, 0 = never expire)
  --start-timeout SECONDS  give up if the server doesn't answer initialize in time (default 60)
  --cache-dir DIR          where discovery replies are cached (default: MCP_LAZY_CACHE_DIR, else the user cache dir)
  --ignore-env PATTERN     leave this environment variable out of the cache key; repeatable; * matches any text
  --version, --help";

pub struct Config {
    pub argv: Vec<String>,
    pub idle: Option<Duration>,
    /// Idle time for a server that has only answered discovery; None means use `idle`.
    pub discovery_idle: Option<Duration>,
    pub max_age: Option<Duration>,
    pub start_timeout: Duration,
    /// None when there is no home directory to put the cache in; then nothing is cached.
    pub cache_dir: Option<PathBuf>,
    /// Extra variable-name patterns to leave out of the cache key.
    pub ignore_env: Vec<String>,
}

enum Cmd {
    Run(Config),
    ClearCache(Option<PathBuf>),
    Help,
    Version,
}

fn main() {
    match parse(std::env::args().skip(1).collect()) {
        Ok(Cmd::Run(cfg)) => std::process::exit(proxy::run(cfg)),
        Ok(Cmd::ClearCache(None)) => {
            note!("mcp-lazy: no cache directory: HOME is not set and this user has no home directory");
            std::process::exit(1);
        }
        Ok(Cmd::ClearCache(Some(dir))) => match cache::clear(&dir) {
            Ok(n) => say!("removed {n} cache files from {}", dir.display()),
            Err(e) => {
                note!("mcp-lazy: {e}");
                std::process::exit(1);
            }
        },
        Ok(Cmd::Help) => say!("{USAGE}"),
        Ok(Cmd::Version) => say!("mcp-lazy {}", env!("CARGO_PKG_VERSION")),
        Err(e) => {
            note!("mcp-lazy: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    }
}

fn parse(args: Vec<String>) -> Result<Cmd, String> {
    let mut idle = Some(Duration::from_secs(600));
    let mut discovery_idle = Some(Duration::from_secs(10));
    let mut max_age = Some(Duration::from_secs(7 * 24 * 3600));
    let mut start_timeout = Duration::from_secs(60);
    let mut cache_dir = None;
    let mut ignore_env = Vec::new();
    let mut clear = false;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--" => {
                let argv: Vec<String> = it.collect();
                if argv.is_empty() {
                    return Err("no server command after --".into());
                }
                if clear {
                    return Err("clear-cache takes no server command".into());
                }
                return Ok(Cmd::Run(Config {
                    argv,
                    idle,
                    discovery_idle,
                    max_age,
                    start_timeout,
                    cache_dir: cache_dir.or_else(default_cache_dir),
                    ignore_env,
                }));
            }
            "--idle" => idle = secs(it.next(), "--idle")?,
            "--discovery-idle" => discovery_idle = secs(it.next(), "--discovery-idle")?,
            "--max-age" => max_age = secs(it.next(), "--max-age")?,
            "--start-timeout" => {
                start_timeout =
                    secs(it.next(), "--start-timeout")?.ok_or("--start-timeout must be more than 0")?
            }
            "--cache-dir" => {
                cache_dir = Some(PathBuf::from(it.next().ok_or("--cache-dir needs a value")?))
            }
            "--ignore-env" => ignore_env.push(it.next().ok_or("--ignore-env needs a value")?),
            "clear-cache" => clear = true,
            "--help" | "-h" => return Ok(Cmd::Help),
            "--version" | "-V" => return Ok(Cmd::Version),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    if clear {
        return Ok(Cmd::ClearCache(cache_dir.or_else(default_cache_dir)));
    }
    Err("missing -- and the server command".into())
}

fn secs(v: Option<String>, flag: &str) -> Result<Option<Duration>, String> {
    let v = v.ok_or_else(|| format!("{flag} needs a value"))?;
    let n: u64 = v.parse().map_err(|_| format!("{flag} takes whole seconds, got {v:?}"))?;
    Ok((n > 0).then(|| Duration::from_secs(n)))
}

/// The cache directory, or None when there is no home directory to put it in.
fn default_cache_dir() -> Option<PathBuf> {
    let set = |name| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(d) = set("MCP_LAZY_CACHE_DIR") {
        return Some(d);
    }
    if cfg!(not(target_os = "macos")) {
        if let Some(x) = set("XDG_CACHE_HOME") {
            return Some(x.join("mcp-lazy"));
        }
    }
    let home = set("HOME").or_else(sys::home_dir)?;
    if cfg!(target_os = "macos") {
        Some(home.join("Library/Caches/mcp-lazy"))
    } else {
        Some(home.join(".cache/mcp-lazy"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Result<Cmd, String> {
        parse(args.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn parses_flags_and_command() {
        let Ok(Cmd::Run(c)) = run(&[
            "--idle", "0", "--discovery-idle", "3", "--max-age", "5", "--cache-dir", "/x", "--ignore-env", "A_*", "--ignore-env", "B", "--", "npx", "-y",
            "srv", "--idle", "9",
        ]) else {
            panic!("expected Run")
        };
        assert_eq!(c.ignore_env, ["A_*", "B"]);
        assert_eq!(c.idle, None);
        assert_eq!(c.discovery_idle, Some(Duration::from_secs(3)));
        assert_eq!(c.max_age, Some(Duration::from_secs(5)));
        assert_eq!(c.cache_dir, Some(PathBuf::from("/x")));
        // Flags after -- belong to the server.
        assert_eq!(c.argv, ["npx", "-y", "srv", "--idle", "9"]);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(run(&["--"]).is_err());
        assert!(run(&["npx"]).is_err());
        assert!(run(&["--idle", "soon", "--", "x"]).is_err());
        assert!(run(&["--start-timeout", "0", "--", "x"]).is_err());
    }
}
