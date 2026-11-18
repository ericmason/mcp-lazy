//! Disk cache of a server's discovery replies (initialize and the list
//! methods). Never holds tool-call arguments or results.
//!
//! One file per (server command, working directory, environment, protocol
//! version), with variables that change every session left out of the
//! environment. None of those values reach the file: it stores a SHA-256
//! digest of the key, and its name is the first half of that digest. A load
//! uses the file only when the stored digest matches. Files and the directory
//! must belong to this user and not be writable by anyone else.

use crate::sys;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const LIST_METHODS: [&str; 4] = ["tools/list", "prompts/list", "resources/list", "resources/templates/list"];
const VERSION: u32 = 3;
/// Hex digits of the key digest used in the file name: 128 bits.
const NAME_HEX: usize = 32;

/// Variables left out of the cache key because they differ in every session
/// or terminal, which would make every session a miss. `*` matches any text.
const VOLATILE_ENV: &[&str] = &[
    "_",
    "OLDPWD",
    "PWD",
    "SHLVL",
    "WINDOWID",
    "LaunchInstanceID",
    "SSH_CLIENT",
    "SSH_CONNECTION",
    "SSH_TTY",
    "GPG_TTY",
    "TMUX",
    "TMUX_PANE",
    "VSCODE_GIT_IPC_HANDLE",
    "*SESSION*",
    "*_PID",
    "*_WINDOW_ID",
    "*_PANE",
    // Sockets are per process; a server's tools never depend on the path.
    "*_SOCK",
    "*_SOCKET",
    // Claude Code describes its own session in these, and adds new ones.
    "CLAUDE_CODE_*",
    "CLAUDE_PROJECT_DIR",
    // Codello (formerly CodeToGo) sets a bridge socket and token per session;
    // its session ids are covered by *SESSION*.
    "CODELLO_BRIDGE_*",
    "CODETOGO_BRIDGE_*",
];

/// What a cache entry is for. Only its digest is ever written to disk.
#[derive(Clone, PartialEq, Debug)]
pub struct Key {
    pub argv: Vec<String>,
    pub cwd: String,
    /// `env_digest` of the server's environment.
    pub env: String,
    pub protocol_version: String,
}

impl Key {
    /// SHA-256 of every field, each with a length prefix so that no two
    /// different keys hash the same bytes. Lowercase hex.
    pub fn digest(&self) -> String {
        let mut h = Sha256::new();
        let mut field = |b: &[u8]| {
            h.update((b.len() as u64).to_le_bytes());
            h.update(b);
        };
        field(b"mcp-lazy cache key");
        field(&(self.argv.len() as u64).to_le_bytes());
        for a in &self.argv {
            field(a.as_bytes());
        }
        field(self.cwd.as_bytes());
        field(self.env.as_bytes());
        field(self.protocol_version.as_bytes());
        hex(&h.finalize())
    }
}

/// A SHA-256 digest of the environment the server inherits, leaving out the
/// volatile variables and any matching `extra_ignore`.
pub fn env_digest<I: IntoIterator<Item = (String, String)>>(vars: I, extra_ignore: &[String]) -> String {
    let mut kept: Vec<(String, String)> = vars
        .into_iter()
        .filter(|(k, _)| {
            !VOLATILE_ENV.iter().any(|p| glob(p, k)) && !extra_ignore.iter().any(|p| glob(p, k))
        })
        .collect();
    kept.sort();
    let mut h = Sha256::new();
    for (k, v) in kept {
        for part in [k.as_bytes(), v.as_bytes()] {
            h.update((part.len() as u64).to_le_bytes());
            h.update(part);
        }
    }
    hex(&h.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Matches `name` against a pattern in which `*` matches any text.
fn glob(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if name.len() < first.len() + last.len() || !name.starts_with(first) || !name.ends_with(last) {
        return false;
    }
    let mut rest = &name[first.len()..name.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

#[derive(Deserialize)]
struct Entry {
    version: u32,
    /// `Key::digest` of the key this entry is for.
    key: String,
    saved_at: u64,
    initialize: Box<RawValue>,
    #[serde(default)]
    lists: BTreeMap<String, Box<RawValue>>,
}

#[derive(Serialize)]
struct EntryRef<'a> {
    version: u32,
    key: &'a str,
    saved_at: u64,
    initialize: &'a RawValue,
    lists: &'a BTreeMap<String, Box<RawValue>>,
}

pub struct Cache {
    dir: PathBuf,
    /// `Key::digest` of this entry's key.
    digest: String,
    pub initialize: Option<Box<RawValue>>,
    pub lists: BTreeMap<String, Box<RawValue>>,
}

impl Cache {
    /// Loads the entry for `key`. The bool is true on a usable hit.
    pub fn load(dir: &Path, key: Key, max_age: Option<Duration>) -> (Cache, bool) {
        let mut c = Cache { dir: dir.to_path_buf(), digest: key.digest(), initialize: None, lists: BTreeMap::new() };
        match c.read(max_age) {
            Some(e) => {
                c.initialize = Some(e.initialize);
                c.lists = e.lists;
                (c, true)
            }
            None => (c, false),
        }
    }

    fn file_name(&self) -> String {
        format!("{}.json", &self.digest[..NAME_HEX])
    }

    fn read(&self, max_age: Option<Duration>) -> Option<Entry> {
        let dmeta = fs::metadata(&self.dir).ok()?;
        if !dmeta.is_dir() || !trusted(&dmeta) {
            return None;
        }
        let path = self.dir.join(self.file_name());
        // Check the file that was opened, not the path, so nothing can swap
        // it in between. O_NOFOLLOW refuses a symlink; O_NONBLOCK keeps a
        // FIFO from blocking the open.
        let mut f = match fs::OpenOptions::new().read(true).custom_flags(sys::O_NOFOLLOW | sys::O_NONBLOCK).open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
            Err(_) => return self.untrusted(&path),
        };
        match f.metadata() {
            Ok(meta) if meta.is_file() && trusted(&meta) => {}
            _ => return self.untrusted(&path),
        }
        let mut text = String::new();
        f.read_to_string(&mut text).ok()?;
        let e: Entry = serde_json::from_str(&text).ok()?;
        if e.version != VERSION || e.key != self.digest {
            return None;
        }
        if let Some(m) = max_age {
            if now_secs().saturating_sub(e.saved_at) > m.as_secs() {
                return None;
            }
        }
        Some(e)
    }

    fn untrusted(&self, path: &Path) -> Option<Entry> {
        note!("mcp-lazy: ignoring {} (not a regular file owned by you, or writable by others)", path.display());
        None
    }

    /// Stores a fresh initialize result. Returns true if it changed.
    pub fn set_initialize(&mut self, raw: &RawValue) -> bool {
        if self.initialize.as_deref().map(RawValue::get) == Some(raw.get()) {
            return false;
        }
        self.initialize = Some(own(raw));
        true
    }

    /// Stores a fresh first page of a list method. Returns true if it changed.
    pub fn set_list(&mut self, method: &str, raw: &RawValue) -> bool {
        if self.lists.get(method).map(|v| v.get()) == Some(raw.get()) {
            return false;
        }
        self.lists.insert(method.to_string(), own(raw));
        true
    }

    /// Writes the entry atomically at mode 600. A no-op until initialize is known.
    pub fn save(&self) -> io::Result<()> {
        let Some(init) = self.initialize.as_deref() else { return Ok(()) };
        let body = serde_json::to_vec(&EntryRef {
            version: VERSION,
            key: &self.digest,
            saved_at: now_secs(),
            initialize: init,
            lists: &self.lists,
        })?;
        fs::DirBuilder::new().recursive(true).mode(0o700).create(&self.dir)?;
        if !trusted(&fs::metadata(&self.dir)?) {
            return Err(io::Error::other(format!("{} is not owned by you or is writable by others", self.dir.display())));
        }
        let name = self.file_name();
        let tmp = self.dir.join(format!(".{name}.{}.tmp", std::process::id()));
        let _ = fs::remove_file(&tmp);
        let mut f =
            fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(sys::O_NOFOLLOW).open(&tmp)?;
        let done = f.write_all(&body).and_then(|_| {
            drop(f);
            fs::rename(&tmp, self.dir.join(name))
        });
        if done.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        done
    }
}

/// Removes `path` if it's a regular file owned by this user. Returns whether
/// it did; a file that's already gone counts as not removed, not an error.
fn remove_if_ours(path: &Path) -> io::Result<bool> {
    let gone = |e: io::Error| if e.kind() == io::ErrorKind::NotFound { Ok(false) } else { Err(e) };
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) => return gone(e),
    };
    if !meta.is_file() || meta.uid() != sys::euid() {
        return Ok(false);
    }
    fs::remove_file(path).map(|_| true).or_else(gone)
}

/// Deletes the cache files in `dir`: only names that this version or an
/// earlier one writes, and only regular files owned by this user. Refuses a
/// directory that fails the same ownership check a load does. Returns how
/// many files it removed.
pub fn clear(dir: &Path) -> io::Result<usize> {
    let dmeta = match fs::metadata(dir) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    if !dmeta.is_dir() || !trusted(&dmeta) {
        return Err(io::Error::other(format!(
            "not clearing {}: not a directory owned by you, or writable by others",
            dir.display()
        )));
    }
    let mut n = 0;
    for e in fs::read_dir(dir)? {
        let e = e?;
        let Some(name) = e.file_name().to_str().map(String::from) else { continue };
        if !is_cache_file_name(&name) {
            continue;
        }
        // Another mcp-lazy may remove or rename a file while this runs.
        if remove_if_ours(&e.path())? {
            n += 1;
        }
    }
    Ok(n)
}

/// True for `<hex>.json` and `.<hex>.json.<pid>.tmp`, where the hex is 32
/// digits (this version) or 16 (earlier versions).
fn is_cache_file_name(name: &str) -> bool {
    let is_entry = |s: &str| {
        s.strip_suffix(".json").is_some_and(|h| {
            (h.len() == NAME_HEX || h.len() == 16) && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        })
    };
    if let Some(tmp) = name.strip_prefix('.').and_then(|s| s.strip_suffix(".tmp")) {
        return tmp
            .rsplit_once('.')
            .is_some_and(|(entry, pid)| is_entry(entry) && !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()));
    }
    is_entry(name)
}

fn own(raw: &RawValue) -> Box<RawValue> {
    RawValue::from_string(raw.get().to_string()).expect("RawValue is valid JSON")
}

fn trusted(meta: &fs::Metadata) -> bool {
    meta.uid() == sys::euid() && meta.mode() & 0o022 == 0
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mcp-lazy-unit-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn key(cwd: &str) -> Key {
        Key { argv: vec!["srv".into()], cwd: cwd.into(), env: "0".into(), protocol_version: "2025-06-18".into() }
    }

    fn vars(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn env_digest_ignores_session_variables_but_not_settings() {
        let a = env_digest(vars(&[("TOOLSET", "a"), ("CLAUDE_CODE_SESSION_ID", "1"), ("CLAUDE_PID", "5")]), &[]);
        let b = env_digest(vars(&[("CLAUDE_PID", "6"), ("CLAUDE_CODE_SESSION_ID", "2"), ("TOOLSET", "a")]), &[]);
        assert_eq!(a, b, "session ids and order must not matter");
        let codello = |n: &str| {
            env_digest(
                vars(&[
                    ("TOOLSET", "a"),
                    ("CODELLO_BRIDGE_TOKEN", n),
                    ("CODELLO_BRIDGE_SOCKET", n),
                    ("CLAUDE_PROJECT_DIR", n),
                    ("SSH_AUTH_SOCK", n),
                ]),
                &[],
            )
        };
        assert_eq!(codello("1"), codello("2"), "per-session bridge, project, and socket variables must not matter");
        let c = env_digest(vars(&[("TOOLSET", "b"), ("CLAUDE_CODE_SESSION_ID", "1")]), &[]);
        assert_ne!(a, c, "a changed setting must change the key");
        let d = env_digest(vars(&[("TOOLSET", "b"), ("CLAUDE_CODE_SESSION_ID", "1")]), &["TOOL*".into()]);
        let e = env_digest(vars(&[("TOOLSET", "a")]), &["TOOL*".into()]);
        assert_eq!(d, e);
        // The separator keeps ("AB","C") and ("A","BC") apart.
        assert_ne!(env_digest(vars(&[("AB", "C")]), &[]), env_digest(vars(&[("A", "BC")]), &[]));
    }

    #[test]
    fn globs() {
        assert!(glob("*SESSION*", "TERM_SESSION_ID"));
        assert!(glob("*SESSION*", "SESSION"));
        assert!(glob("*_PID", "CLAUDE_PID"));
        assert!(!glob("*_PID", "PIDFILE"));
        assert!(glob("A*B*C", "AxxBxxC"));
        assert!(!glob("A*B*C", "AC"));
        assert!(!glob("AB*BA", "ABA"));
        assert!(glob("PATH", "PATH") && !glob("PATH", "PATHS"));
    }

    fn raw(s: &str) -> Box<RawValue> {
        RawValue::from_string(s.into()).unwrap()
    }

    #[test]
    fn round_trips_and_sets_mode_600() {
        let d = tmpdir("rt");
        let (mut c, hit) = Cache::load(&d, key("/a"), None);
        assert!(!hit);
        assert!(c.set_initialize(&raw(r#"{"v":1}"#)));
        assert!(!c.set_initialize(&raw(r#"{"v":1}"#)));
        assert!(c.set_list("tools/list", &raw(r#"{"tools":[]}"#)));
        c.save().unwrap();
        let (c2, hit) = Cache::load(&d, key("/a"), None);
        assert!(hit);
        assert_eq!(c2.lists["tools/list"].get(), r#"{"tools":[]}"#);
        let f = fs::read_dir(&d).unwrap().next().unwrap().unwrap().path();
        assert_eq!(fs::metadata(&f).unwrap().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(&d).unwrap().mode() & 0o777, 0o700);
    }

    #[test]
    fn other_keys_miss() {
        let d = tmpdir("keys");
        let (mut c, _) = Cache::load(&d, key("/a"), None);
        c.set_initialize(&raw("{}"));
        c.save().unwrap();
        assert!(!Cache::load(&d, key("/b"), None).1);
    }

    #[test]
    fn ignores_files_others_can_write() {
        let d = tmpdir("perm");
        let (mut c, _) = Cache::load(&d, key("/a"), None);
        c.set_initialize(&raw("{}"));
        c.save().unwrap();
        let f = fs::read_dir(&d).unwrap().next().unwrap().unwrap().path();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(!Cache::load(&d, key("/a"), None).1);
    }

    #[test]
    fn ignores_a_directory_others_can_write() {
        let d = tmpdir("dirperm");
        let (mut c, _) = Cache::load(&d, key("/a"), None);
        c.set_initialize(&raw("{}"));
        c.save().unwrap();
        fs::set_permissions(&d, fs::Permissions::from_mode(0o777)).unwrap();
        let hit = Cache::load(&d, key("/a"), None).1;
        fs::set_permissions(&d, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(!hit);
    }

    #[test]
    fn a_file_whose_stored_key_differs_is_a_miss() {
        let d = tmpdir("collide");
        let (mut b, _) = Cache::load(&d, key("/b"), None);
        b.set_initialize(&raw("{}"));
        b.save().unwrap();
        // Put /b's entry where /a's would go, as a hash collision would.
        let (a, _) = Cache::load(&d, key("/a"), None);
        fs::rename(d.join(b.file_name()), d.join(a.file_name())).unwrap();
        assert!(!Cache::load(&d, key("/a"), None).1);
    }

    #[test]
    fn expires_old_entries() {
        let d = tmpdir("age");
        let (mut c, _) = Cache::load(&d, key("/a"), None);
        c.set_initialize(&raw("{}"));
        c.save().unwrap();
        let f = fs::read_dir(&d).unwrap().next().unwrap().unwrap().path();
        let text = fs::read_to_string(&f).unwrap().replace("\"saved_at\":", "\"saved_at\":1,\"x\":");
        fs::write(&f, text).unwrap();
        assert!(!Cache::load(&d, key("/a"), Some(Duration::from_secs(60))).1);
        assert!(Cache::load(&d, key("/a"), None).1);
    }

    #[test]
    fn the_digest_keeps_argument_boundaries() {
        let k = |argv: &[&str]| Key { argv: argv.iter().map(|a| a.to_string()).collect(), ..key("/a") };
        assert_ne!(k(&["a b"]).digest(), k(&["a", "b"]).digest());
        assert_ne!(k(&["ab", ""]).digest(), k(&["a", "b"]).digest());
        assert_eq!(k(&["a", "b"]).digest(), k(&["a", "b"]).digest());
        assert_eq!(k(&["a"]).digest().len(), 64);
    }

    #[test]
    fn the_file_holds_no_part_of_the_key() {
        let d = tmpdir("nokey");
        let k = Key { argv: vec!["srv".into(), "--api-key=s3cret".into()], ..key("/home/someone/project") };
        let (mut c, _) = Cache::load(&d, k.clone(), None);
        c.set_initialize(&raw("{}"));
        c.save().unwrap();
        let f = fs::read_dir(&d).unwrap().next().unwrap().unwrap().path();
        let body = fs::read_to_string(&f).unwrap();
        for s in ["s3cret", "srv", "/home/someone", "2025-06-18"] {
            assert!(!body.contains(s), "{s:?} is in {body}");
        }
        assert!(body.contains(&k.digest()));
        assert!(Cache::load(&d, k, None).1);
    }

    #[test]
    fn clear_removes_only_cache_files() {
        let d = tmpdir("clear");
        let (mut c, _) = Cache::load(&d, key("/a"), None);
        c.set_initialize(&raw("{}"));
        c.save().unwrap();
        let keep = ["package.json", "notes.tmp", "0123.json", ".x.json.1.tmp", "0123456789abcdeg.json"];
        let old = ["0123456789abcdef.json", ".0123456789abcdef.json.42.tmp"];
        for n in keep.iter().chain(&old) {
            fs::write(d.join(n), "{}").unwrap();
        }
        assert_eq!(clear(&d).unwrap(), 3);
        let mut left: Vec<String> =
            fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        left.sort();
        let mut want: Vec<String> = keep.iter().map(|s| s.to_string()).collect();
        want.sort();
        assert_eq!(left, want);
    }

    #[test]
    fn clear_refuses_a_directory_others_can_write() {
        let d = tmpdir("clear-perm");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("0123456789abcdef.json"), "{}").unwrap();
        fs::set_permissions(&d, fs::Permissions::from_mode(0o777)).unwrap();
        let r = clear(&d);
        fs::set_permissions(&d, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(r.is_err());
        assert!(d.join("0123456789abcdef.json").exists());
    }

    #[test]
    fn a_symlinked_cache_file_is_ignored() {
        let d = tmpdir("symlink");
        let (mut c, _) = Cache::load(&d, key("/a"), None);
        c.set_initialize(&raw("{}"));
        c.save().unwrap();
        let f = d.join(c.file_name());
        let real = d.join("real");
        fs::rename(&f, &real).unwrap();
        std::os::unix::fs::symlink(&real, &f).unwrap();
        assert!(!Cache::load(&d, key("/a"), None).1);
    }

    #[test]
    fn a_file_that_vanished_counts_as_not_removed() {
        let d = tmpdir("vanished");
        fs::create_dir_all(&d).unwrap();
        assert!(!remove_if_ours(&d.join("0123456789abcdef.json")).unwrap());
        fs::write(d.join("0123456789abcdef.json"), "{}").unwrap();
        assert!(remove_if_ours(&d.join("0123456789abcdef.json")).unwrap());
    }

    #[test]
    fn a_failed_save_leaves_no_temp_file() {
        let d = tmpdir("failed-save");
        let (mut c, _) = Cache::load(&d, key("/a"), None);
        c.set_initialize(&raw("{}"));
        // A non-empty directory where the file goes makes the rename fail.
        fs::create_dir_all(d.join(c.file_name()).join("x")).unwrap();
        assert!(c.save().is_err());
        let names: Vec<String> =
            fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        assert_eq!(names, [c.file_name()]);
    }

    #[test]
    fn save_without_initialize_writes_nothing() {
        let d = tmpdir("noinit");
        let (c, _) = Cache::load(&d, key("/a"), None);
        c.save().unwrap();
        assert!(!d.exists());
    }
}
