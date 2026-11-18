//! End-to-end tests: the real mcp-lazy binary in front of tests/fixtures/fake_server.py.

use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake_server.py");
const BIN: &str = env!("CARGO_BIN_EXE_mcp-lazy");
const WAIT: Duration = Duration::from_secs(10);

struct Env {
    dir: PathBuf,
}

impl Env {
    fn new(name: &str) -> Env {
        let dir = std::env::temp_dir().join(format!("mcp-lazy-it-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Env { dir }
    }

    fn cache(&self) -> PathBuf {
        self.dir.join("cache")
    }

    fn log(&self) -> Vec<String> {
        fs::read_to_string(self.dir.join("log")).unwrap_or_default().lines().map(String::from).collect()
    }

    fn pids(&self, prefix: &str) -> Vec<u32> {
        self.log().iter().filter_map(|l| l.strip_prefix(prefix)).map(|p| p.trim().parse().unwrap()).collect()
    }

    fn starts(&self) -> usize {
        self.pids("start ").len()
    }

    fn session(&self, args: &[&str], tools: &str) -> Session {
        self.session_env(args, tools, &[])
    }

    fn session_env(&self, args: &[&str], tools: &str, vars: &[(&str, &str)]) -> Session {
        self.session_full(args, &[], tools, vars)
    }

    /// `server_args` go after the fixture path; the fixture ignores them.
    fn session_full(&self, args: &[&str], server_args: &[&str], tools: &str, vars: &[(&str, &str)]) -> Session {
        self.session_cmd(Command::new(BIN), args, server_args, tools, vars)
    }

    /// Like `session_full`, but runs mcp-lazy through `cmd`, which must end
    /// by running mcp-lazy with the arguments added after it.
    fn session_cmd(
        &self,
        mut cmd: Command,
        args: &[&str],
        server_args: &[&str],
        tools: &str,
        vars: &[(&str, &str)],
    ) -> Session {
        let mut child = self
            .command(&mut cmd, args, server_args, tools, vars)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(serde_json::from_str::<Value>(&line).expect("proxy wrote invalid JSON")).is_err() {
                    break;
                }
            }
        });
        let stdin = child.stdin.take();
        Session { child, stdin, rx, stash: Vec::new() }
    }

    fn command<'a>(
        &self,
        cmd: &'a mut Command,
        args: &[&str],
        server_args: &[&str],
        tools: &str,
        vars: &[(&str, &str)],
    ) -> &'a mut Command {
        cmd.arg("--cache-dir")
            .arg(self.cache())
            // Tests vary the fixture's settings without wanting a new cache entry.
            .args(["--ignore-env", "FAKE_*"])
            .args(args)
            .args(["--", "python3", FIXTURE])
            .args(server_args)
            .env("FAKE_LOG", self.dir.join("log"))
            .env("FAKE_TOOLS", tools)
            .envs(vars.iter().copied())
    }

    /// Runs one full session so the cache is warm, and returns its tool names.
    fn warm(&self, tools: &str) -> Vec<String> {
        let mut s = self.session(&[], tools);
        let names = tool_names(&s.handshake());
        s.close();
        names
    }
}

struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<Value>,
    stash: Vec<Value>,
}

impl Session {
    fn send(&mut self, v: Value) {
        let s = self.stdin.as_mut().unwrap();
        writeln!(s, "{v}").unwrap();
        s.flush().unwrap();
    }

    fn recv_where(&mut self, what: &str, f: impl Fn(&Value) -> bool) -> Value {
        if let Some(i) = self.stash.iter().position(&f) {
            return self.stash.remove(i);
        }
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(v) if f(&v) => return v,
                Ok(v) => self.stash.push(v),
                Err(_) => panic!("timed out waiting for {what}; got {:?}", self.stash),
            }
        }
    }

    fn recv_id(&mut self, id: Value) -> Value {
        let what = format!("id {id}");
        self.recv_where(&what, |v| v.get("id") == Some(&id) && v.get("method").is_none())
    }

    fn handshake(&mut self) -> Value {
        self.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-06-18","capabilities":{"roots":{}},"clientInfo":{"name":"it","version":"0"}}}));
        let init = self.recv_id(json!(1));
        assert_eq!(init["result"]["serverInfo"]["name"], "fake");
        self.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        self.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}));
        self.recv_id(json!(2))
    }

    fn call(&mut self, id: i64, name: &str, args: Value) -> Value {
        self.send_call(id, name, args);
        self.recv_id(json!(id))
    }

    fn send_call(&mut self, id: i64, name: &str, args: Value) {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":args}}));
    }

    /// Closes stdin and waits for the proxy to exit.
    fn close(&mut self) {
        self.stdin.take();
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            if self.child.try_wait().unwrap().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("proxy did not exit after stdin closed");
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn tool_names(list: &Value) -> Vec<String> {
    list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
}

fn text_of(reply: &Value) -> String {
    reply["result"]["content"][0]["text"].as_str().unwrap_or_else(|| panic!("no text in {reply}")).to_string()
}

fn alive(pid: u32) -> bool {
    Command::new("kill").args(["-0", &pid.to_string()]).stderr(Stdio::null()).status().unwrap().success()
}

fn wait_dead(pid: u32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !alive(pid) {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn first_session_starts_the_server_and_writes_a_private_cache() {
    let env = Env::new("first");
    assert_eq!(env.warm("echo"), ["echo"]);
    assert_eq!(env.starts(), 1);
    let files: Vec<_> = fs::read_dir(env.cache()).unwrap().map(|e| e.unwrap().path()).collect();
    assert_eq!(files.len(), 1);
    assert_eq!(fs::metadata(&files[0]).unwrap().mode() & 0o777, 0o600);
    let body = fs::read_to_string(&files[0]).unwrap();
    assert!(body.contains("\"tools/list\""));
}

#[test]
fn cached_session_answers_discovery_without_starting_the_server() {
    let env = Env::new("cached");
    env.warm("echo");
    let mut s = env.session(&[], "echo");
    assert_eq!(tool_names(&s.handshake()), ["echo"]);
    s.send(json!({"jsonrpc":"2.0","id":3,"method":"ping"}));
    assert_eq!(s.recv_id(json!(3))["result"], json!({}));
    assert_eq!(env.starts(), 1, "discovery must not start the server");

    assert_eq!(text_of(&s.call(4, "echo", json!({"text":"hi"}))), "hi");
    assert_eq!(env.starts(), 2);
    // The replayed initialize carries the client's own parameters.
    let inits: Vec<_> = env.log().into_iter().filter(|l| l.starts_with("init ")).collect();
    assert_eq!(inits.len(), 2);
    assert_eq!(inits[0], inits[1]);
    assert!(inits[1].contains("\"name\": \"it\""));
    s.close();
}

#[test]
fn concurrent_calls_during_startup_start_one_server_and_all_get_answers() {
    let env = Env::new("queue");
    env.warm("echo");
    let mut s = env.session(&[], "echo");
    s.handshake();
    for id in 20..25 {
        s.send_call(id, "echo", json!({"text": id.to_string()}));
    }
    for id in 20..25 {
        assert_eq!(text_of(&s.recv_id(json!(id))), id.to_string());
    }
    assert_eq!(env.starts(), 2);
    s.close();
}

#[test]
fn idle_server_is_stopped_and_restarted_on_the_next_call() {
    let env = Env::new("idle");
    env.warm("echo");
    let mut s = env.session(&["--idle", "1"], "echo");
    s.handshake();
    assert_eq!(text_of(&s.call(5, "echo", json!({"text":"a"}))), "a");
    let first = *env.pids("start ").last().unwrap();
    assert!(wait_dead(first), "idle server was not stopped");
    // Discovery still comes from the cache while it's stopped.
    s.send(json!({"jsonrpc":"2.0","id":6,"method":"tools/list"}));
    assert_eq!(tool_names(&s.recv_id(json!(6))), ["echo"]);
    assert_eq!(text_of(&s.call(7, "echo", json!({"text":"b"}))), "b");
    assert_eq!(env.starts(), 3);
    s.close();
}

#[test]
fn a_slow_call_is_not_cut_off_by_the_idle_timer() {
    let env = Env::new("slow");
    env.warm("echo");
    let mut s = env.session(&["--idle", "1"], "echo");
    s.handshake();
    assert_eq!(text_of(&s.call(8, "echo", json!({"text":"slow","sleep":2.5}))), "slow");
    s.close();
}

#[test]
fn a_changed_tool_list_updates_the_cache_and_notifies_the_client() {
    let env = Env::new("changed");
    env.warm("echo");
    let mut s = env.session(&[], "echo,extra");
    assert_eq!(tool_names(&s.handshake()), ["echo"], "first answer comes from the old cache");
    s.call(9, "echo", json!({"text":"x"}));
    s.recv_where("tools/list_changed", |v| v["method"] == "notifications/tools/list_changed");
    s.send(json!({"jsonrpc":"2.0","id":10,"method":"tools/list"}));
    assert_eq!(tool_names(&s.recv_id(json!(10))), ["echo", "extra"]);
    s.close();
    let starts = env.starts();
    assert_eq!(env.warm("echo,extra"), ["echo", "extra"]);
    assert_eq!(env.starts(), starts, "the updated cache answers without a start");
}

#[test]
fn a_crash_fails_the_call_in_flight_and_the_next_call_recovers() {
    let env = Env::new("crash");
    env.warm("echo");
    let mut s = env.session(&[], "echo");
    s.handshake();
    let r = s.call(11, "crash", json!({}));
    assert_eq!(r["error"]["code"], -32603, "{r}");
    assert_eq!(text_of(&s.call(12, "echo", json!({"text":"back"}))), "back");
    s.close();
}

#[test]
fn server_requests_reach_the_client_and_replies_reach_the_server() {
    let env = Env::new("roots");
    env.warm("echo");
    let mut s = env.session(&[], "echo");
    s.handshake();
    s.send_call(13, "ask_roots", json!({}));
    let req = s.recv_where("roots/list", |v| v["method"] == "roots/list");
    s.send(json!({"jsonrpc":"2.0","id":req["id"],"result":{"roots":[]}}));
    assert_eq!(text_of(&s.recv_id(json!(13))), r#"{"roots": []}"#);
    s.close();
}

#[test]
fn closing_the_client_stops_the_server_and_its_children() {
    let env = Env::new("eof");
    env.warm("echo");
    let mut s = env.session(&[], "echo");
    s.handshake();
    s.call(14, "spawn_child", json!({}));
    let server = *env.pids("start ").last().unwrap();
    let grandchild = env.pids("child ")[0];
    assert!(alive(grandchild));
    s.close();
    assert!(wait_dead(server), "server still running");
    assert!(wait_dead(grandchild), "server's child still running");
}

#[test]
fn a_cache_file_others_can_write_is_ignored() {
    let env = Env::new("perm");
    env.warm("echo");
    for e in fs::read_dir(env.cache()).unwrap() {
        fs::set_permissions(e.unwrap().path(), fs::Permissions::from_mode(0o666)).unwrap();
    }
    env.warm("echo");
    assert_eq!(env.starts(), 2, "an untrusted cache must not be used");
}

#[test]
fn a_cancelled_queued_call_is_never_sent() {
    let env = Env::new("cancel");
    env.warm("echo");
    // The server takes a second to start, so all three messages arrive while it's starting.
    let mut s = env.session_env(&[], "echo", &[("FAKE_START_DELAY", "1")]);
    s.handshake();
    s.send_call(15, "echo", json!({"text":"keep"}));
    s.send_call(16, "echo", json!({"text":"drop"}));
    s.send(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":16}}));
    assert_eq!(text_of(&s.recv_id(json!(15))), "keep");
    thread::sleep(Duration::from_millis(500));
    while let Ok(v) = s.rx.try_recv() {
        s.stash.push(v);
    }
    assert!(s.stash.iter().all(|v| v.get("id") != Some(&json!(16))), "cancelled call was answered: {:?}", s.stash);
    s.close();
    let calls = env.log().iter().filter(|l| *l == "recv tools/call").count();
    assert_eq!(calls, 1, "the cancelled call reached the server");
}

#[test]
fn a_slow_call_on_a_settled_server_is_not_cut_off_by_the_idle_timer() {
    let env = Env::new("slow-settled");
    env.warm("echo");
    let mut s = env.session(&["--idle", "1"], "echo");
    s.handshake();
    // The first call starts the server; wait for the list refresh that follows it to finish,
    // so the slow call is the only thing in flight.
    assert_eq!(text_of(&s.call(30, "echo", json!({"text":"a"}))), "a");
    thread::sleep(Duration::from_millis(300));
    assert_eq!(text_of(&s.call(31, "echo", json!({"text":"slow","sleep":2.5}))), "slow");
    assert_eq!(env.starts(), 2, "the server was restarted during the slow call");
    s.close();
}

#[test]
fn messages_from_a_stopped_server_are_ignored_after_a_restart() {
    let env = Env::new("oldgen");
    env.warm("echo");
    // The server ignores SIGTERM and lingers 1 second after stdin closes, then writes a
    // notification and exits, so its last line and its EOF arrive after the next start.
    let mut s = env.session_env(&["--idle", "1"], "echo", &[("FAKE_LINGER", "1")]);
    s.handshake();
    assert_eq!(text_of(&s.call(32, "echo", json!({"text":"a"}))), "a");
    // The idle stop begins about 1 second after the last server message and lasts about 1 second.
    thread::sleep(Duration::from_millis(1500));
    let r = s.call(33, "echo", json!({"text":"b"}));
    assert_eq!(text_of(&r), "b", "{r}");
    assert_eq!(env.starts(), 3);
    thread::sleep(Duration::from_millis(300));
    while let Ok(v) = s.rx.try_recv() {
        s.stash.push(v);
    }
    assert!(
        s.stash.iter().all(|v| v["params"]["data"] != "old server"),
        "a stopped server's message reached the client: {:?}",
        s.stash
    );
    s.close();
}

#[test]
fn a_list_request_with_a_cursor_goes_to_the_server() {
    let env = Env::new("cursor");
    env.warm("echo");
    let mut s = env.session(&[], "echo");
    s.handshake();
    assert_eq!(env.starts(), 1);
    s.send(json!({"jsonrpc":"2.0","id":34,"method":"tools/list","params":{"cursor":"page2"}}));
    s.recv_id(json!(34));
    assert_eq!(env.starts(), 2, "a later page was answered from the cache");
    s.close();
}

#[test]
fn a_request_before_initialize_with_no_cache_is_answered() {
    let env = Env::new("preinit");
    let mut s = env.session(&[], "echo");
    s.send(json!({"jsonrpc":"2.0","id":40,"method":"tools/list"}));
    assert_eq!(tool_names(&s.recv_id(json!(40))), ["echo"]);
    s.close();
}

#[test]
fn a_cancelled_call_on_a_running_server_does_not_hold_off_the_idle_stop() {
    let env = Env::new("cancel-up");
    env.warm("echo");
    let mut s = env.session(&["--idle", "1"], "echo");
    s.handshake();
    assert_eq!(text_of(&s.call(41, "echo", json!({"text":"a"}))), "a");
    let server = *env.pids("start ").last().unwrap();
    // The fake server ignores the cancel and would answer after 6 seconds.
    s.send_call(42, "echo", json!({"text":"never","sleep":6}));
    s.send(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":42}}));
    let deadline = Instant::now() + Duration::from_secs(4);
    while alive(server) {
        assert!(Instant::now() < deadline, "the cancelled call kept the server running");
        thread::sleep(Duration::from_millis(50));
    }
    s.close();
}

#[test]
fn a_client_id_in_the_proxy_range_is_rejected() {
    let env = Env::new("reserved");
    env.warm("echo");
    let mut s = env.session(&[], "echo");
    s.handshake();
    s.send(json!({"jsonrpc":"2.0","id":"mcp-lazy:1","method":"tools/call","params":{"name":"echo","arguments":{"text":"x"}}}));
    let r = s.recv_id(json!("mcp-lazy:1"));
    assert_eq!(r["error"]["code"], -32600, "{r}");
    s.close();
}

#[test]
fn server_notifications_do_not_keep_an_idle_server_running() {
    let env = Env::new("heartbeat");
    env.warm("echo");
    let mut s = env.session_env(&["--idle", "1"], "echo", &[("FAKE_HEARTBEAT", "0.2")]);
    s.handshake();
    assert_eq!(text_of(&s.call(43, "echo", json!({"text":"a"}))), "a");
    let server = *env.pids("start ").last().unwrap();
    assert!(wait_dead(server), "a server sending notifications was never stopped for idling");
    s.close();
}

#[test]
fn replies_to_the_proxys_own_refreshes_do_not_keep_an_idle_server_running() {
    let env = Env::new("refresh-heartbeat");
    env.warm("echo");
    // Each list_changed makes the proxy ask for tools/list again.
    let vars = [("FAKE_HEARTBEAT", "0.2"), ("FAKE_HEARTBEAT_METHOD", "notifications/tools/list_changed")];
    let mut s = env.session_env(&["--idle", "1"], "echo", &vars);
    s.handshake();
    assert_eq!(text_of(&s.call(44, "echo", json!({"text":"a"}))), "a");
    let server = *env.pids("start ").last().unwrap();
    assert!(wait_dead(server), "refresh replies kept the server from idling");
    s.close();
}

#[test]
fn a_crash_is_noticed_while_a_child_still_holds_the_server_output() {
    let env = Env::new("crash-child");
    env.warm("echo");
    let mut s = env.session(&[], "echo");
    s.handshake();
    // The grandchild inherits the server's stdout, so the pipe stays open after the crash.
    s.call(50, "spawn_child", json!({}));
    let r = s.call(51, "crash", json!({}));
    assert_eq!(r["error"]["code"], -32603, "{r}");
    assert!(wait_dead(env.pids("child ")[0]), "the crashed server's child still running");
    assert_eq!(text_of(&s.call(52, "echo", json!({"text":"back"}))), "back");
    s.close();
}

#[test]
fn a_different_setting_in_the_environment_gets_its_own_cache_entry() {
    let env = Env::new("env-key");
    env.warm("echo");
    let n = env.starts();
    let mut s = env.session_env(&[], "echo", &[("SERVER_SETTING", "b")]);
    s.handshake();
    assert_eq!(env.starts(), n + 1, "another environment's cache entry was used");
    s.close();
    let mut s = env.session_env(&[], "echo", &[("SERVER_SETTING", "b"), ("CLAUDE_CODE_SESSION_ID", "another")]);
    s.handshake();
    assert_eq!(env.starts(), n + 1, "a per-session variable changed the cache key");
    s.close();
}

#[test]
fn a_request_the_server_cancels_does_not_hold_off_the_idle_stop() {
    let env = Env::new("srv-cancel");
    env.warm("echo");
    let mut s = env.session(&["--idle", "1"], "echo");
    s.handshake();
    s.send_call(53, "ask_then_cancel", json!({}));
    s.recv_where("roots/list", |v| v["method"] == "roots/list");
    assert_eq!(text_of(&s.recv_id(json!(53))), "gave up");
    let server = *env.pids("start ").last().unwrap();
    assert!(wait_dead(server), "a request the server cancelled kept it running");
    s.close();
}

#[test]
fn a_client_cancel_naming_a_proxy_request_is_not_forwarded() {
    let env = Env::new("cancel-proxy-id");
    env.warm("echo");
    let mut s = env.session(&[], "echo");
    s.handshake();
    s.call(54, "echo", json!({"text":"a"}));
    s.send(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":"mcp-lazy:1"}}));
    s.call(55, "echo", json!({"text":"b"}));
    s.close();
    assert!(!env.log().iter().any(|l| l == "recv notifications/cancelled"), "{:?}", env.log());
}

#[test]
fn a_request_with_a_null_id_is_answered() {
    let env = Env::new("null-id");
    env.warm("echo");
    let mut s = env.session(&[], "echo");
    s.handshake();
    s.send(json!({"jsonrpc":"2.0","id":null,"method":"tools/call","params":{"name":"echo","arguments":{"text":"n"}}}));
    let r = s.recv_where("reply to the null id", |v| v.get("id") == Some(&Value::Null) && v.get("method").is_none());
    assert_eq!(text_of(&r), "n");
    s.close();
}

#[test]
fn batches_and_garbage_get_errors_without_starting_the_server() {
    let env = Env::new("batch");
    env.warm("echo");
    let mut s = env.session(&[], "echo");
    s.handshake();
    s.send(json!([{"jsonrpc":"2.0","id":56,"method":"tools/call","params":{"name":"echo","arguments":{"text":"x"}}}]));
    let r = s.recv_where("batch error", |v| v.get("id") == Some(&Value::Null));
    assert_eq!(r["error"]["code"], -32600, "{r}");
    writeln!(s.stdin.as_mut().unwrap(), "not json").unwrap();
    s.stdin.as_mut().unwrap().flush().unwrap();
    let r = s.recv_where("parse error", |v| v.get("id") == Some(&Value::Null));
    assert_eq!(r["error"]["code"], -32700, "{r}");
    assert_eq!(env.starts(), 1, "only the warm-up session started the server");
    s.close();
}

#[test]
fn an_unchanged_refresh_renews_the_cache_age() {
    let env = Env::new("renew");
    env.warm("echo");
    let file = fs::read_dir(env.cache()).unwrap().next().unwrap().unwrap().path();
    let now = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let saved_at = || serde_json::from_str::<Value>(&fs::read_to_string(&file).unwrap()).unwrap()["saved_at"].as_u64().unwrap();
    // Edit only the number: re-serializing would change the stored lists' bytes,
    // and the refresh would then save because something changed.
    let text = fs::read_to_string(&file).unwrap();
    let old = format!("\"saved_at\":{}", saved_at());
    assert!(text.contains(&old));
    fs::write(&file, text.replace(&old, &format!("\"saved_at\":{}", now() - 100))).unwrap();
    assert!(saved_at() + 50 < now());
    let mut s = env.session(&["--max-age", "3600"], "echo");
    s.handshake();
    s.call(57, "echo", json!({"text":"a"}));
    let deadline = Instant::now() + WAIT;
    while saved_at() + 50 < now() {
        assert!(Instant::now() < deadline, "the refresh did not renew saved_at");
        thread::sleep(Duration::from_millis(50));
    }
    s.close();
}

#[test]
fn a_server_started_only_for_discovery_stops_without_waiting_for_idle() {
    let env = Env::new("discovery-stop");
    let mut s = env.session(&["--discovery-idle", "1"], "echo");
    assert_eq!(tool_names(&s.handshake()), ["echo"]);
    let pid = *env.pids("start ").last().unwrap();
    assert!(wait_dead(pid), "a server that only answered discovery stayed up for --idle");
    // The cache it filled answers discovery, and a call starts it again.
    s.send(json!({"jsonrpc":"2.0","id":60,"method":"tools/list"}));
    assert_eq!(tool_names(&s.recv_id(json!(60))), ["echo"]);
    assert_eq!(text_of(&s.call(61, "echo", json!({"text":"back"}))), "back");
    assert_eq!(env.starts(), 2);
    s.close();
}

#[test]
fn a_server_that_ran_a_call_waits_for_the_full_idle() {
    let env = Env::new("discovery-used");
    env.warm("echo");
    let mut s = env.session(&["--discovery-idle", "1"], "echo");
    s.handshake();
    assert_eq!(text_of(&s.call(62, "echo", json!({"text":"a"}))), "a");
    let pid = *env.pids("start ").last().unwrap();
    thread::sleep(Duration::from_secs(3));
    assert!(alive(pid), "a server that ran a call was stopped before --idle");
    s.close();
}

fn cache_files(env: &Env) -> Vec<PathBuf> {
    fs::read_dir(env.cache()).unwrap().map(|e| e.unwrap().path()).collect()
}

#[test]
fn the_cache_file_holds_no_arguments_directory_or_environment() {
    let env = Env::new("no-plain-key");
    let vars = [("SERVER_TOKEN", "env-value-77")];
    let mut s = env.session_full(&[], &["--api-key=s3cret-arg", "postgres://u:pw-in-url@db/x"], "echo", &vars);
    s.handshake();
    s.close();
    let files = cache_files(&env);
    assert_eq!(files.len(), 1);
    let body = fs::read_to_string(&files[0]).unwrap();
    assert!(body.contains("\"tools/list\""));
    let cwd = std::env::current_dir().unwrap();
    for secret in ["s3cret-arg", "pw-in-url", "env-value-77", "fake_server.py", cwd.to_str().unwrap()] {
        assert!(!body.contains(secret), "{secret:?} reached the cache file: {body}");
    }
    // The entry still serves the same command.
    let mut s = env.session_full(&[], &["--api-key=s3cret-arg", "postgres://u:pw-in-url@db/x"], "echo", &vars);
    s.handshake();
    assert_eq!(env.starts(), 1, "the digest-keyed entry was not used");
    s.close();
}

#[test]
fn clear_cache_leaves_other_files_alone() {
    let env = Env::new("clear");
    env.warm("echo");
    fs::write(env.cache().join("package.json"), "{}").unwrap();
    fs::write(env.cache().join("build.tmp"), "").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_mcp-lazy")).arg("clear-cache").arg("--cache-dir").arg(env.cache()).output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let mut left: Vec<String> =
        cache_files(&env).iter().map(|p| p.file_name().unwrap().to_str().unwrap().to_string()).collect();
    left.sort();
    assert_eq!(left, ["build.tmp", "package.json"]);
}

#[test]
fn sigterm_stops_the_server_and_its_children() {
    let env = Env::new("sigterm");
    env.warm("echo");
    let mut s = env.session(&[], "echo");
    s.handshake();
    s.call(70, "spawn_child", json!({}));
    let server = *env.pids("start ").last().unwrap();
    let grandchild = env.pids("child ")[0];
    let st = Command::new("kill").args(["-TERM", &s.child.id().to_string()]).status().unwrap();
    assert!(st.success());
    assert!(wait_dead(server), "server still running after SIGTERM to mcp-lazy");
    assert!(wait_dead(grandchild), "server's child still running after SIGTERM to mcp-lazy");
    let deadline = Instant::now() + WAIT;
    let status = loop {
        if let Some(st) = s.child.try_wait().unwrap() {
            break st;
        }
        assert!(Instant::now() < deadline, "mcp-lazy did not exit after SIGTERM");
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(128 + 15), "{status:?}");
}

#[test]
fn a_server_that_stops_reading_does_not_freeze_the_proxy() {
    let env = Env::new("no-read");
    env.warm("echo");
    let mut s = env.session(&[], "echo");
    s.handshake();
    assert_eq!(text_of(&s.call(80, "stop_reading", json!({}))), "ok");
    // Far more than a pipe holds, so a blocking write to the server never returns.
    s.send_call(81, "echo", json!({"text": "y".repeat(4 << 20)}));
    // The proxy still answers what it answers itself.
    writeln!(s.stdin.as_mut().unwrap(), "not json").unwrap();
    s.stdin.as_mut().unwrap().flush().unwrap();
    let r = s.recv_where("parse error", |v| v.get("id") == Some(&Value::Null));
    assert_eq!(r["error"]["code"], -32700, "{r}");
    let server = *env.pids("start ").last().unwrap();
    s.close();
    assert!(wait_dead(server), "server still running");
}

#[test]
fn a_signal_ignored_at_start_stays_ignored() {
    let env = Env::new("sig-ign");
    env.warm("echo");
    // Like nohup: SIGHUP is ignored before mcp-lazy starts.
    let mut sh = Command::new("sh");
    sh.args(["-c", "trap '' HUP; exec \"$0\" \"$@\"", BIN]);
    let mut s = env.session_cmd(sh, &[], &[], "echo", &[]);
    s.handshake();
    let pid = s.child.id().to_string();
    assert!(Command::new("kill").args(["-HUP", &pid]).status().unwrap().success());
    assert_eq!(text_of(&s.call(94, "echo", json!({"text":"still here"}))), "still here");
    assert!(s.child.try_wait().unwrap().is_none(), "mcp-lazy exited on an ignored SIGHUP");
    // Signals that weren't ignored are still caught.
    assert!(Command::new("kill").args(["-TERM", &pid]).status().unwrap().success());
    let deadline = Instant::now() + WAIT;
    let status = loop {
        if let Some(st) = s.child.try_wait().unwrap() {
            break st;
        }
        assert!(Instant::now() < deadline, "mcp-lazy did not exit after SIGTERM");
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(128 + 15), "{status:?}");
}

#[test]
fn requests_to_a_server_that_stops_reading_get_errors_once_its_queue_is_full() {
    let env = Env::new("server-full");
    env.warm("echo");
    let mut s = env.session(&[], "echo");
    s.handshake();
    assert_eq!(text_of(&s.call(95, "stop_reading", json!({}))), "ok");
    // More 4 KiB calls than the queue and the pipe hold together.
    let text = "q".repeat(4096);
    for id in 1000..3500 {
        s.send_call(id, "echo", json!({ "text": text }));
    }
    let r = s.recv_where("a full-queue error", |v| v["error"]["code"] == -32603);
    assert!(r["error"]["message"].as_str().unwrap().contains("could not write"), "{r}");
    let server = *env.pids("start ").last().unwrap();
    s.close();
    assert!(wait_dead(server), "server still running");
}

#[test]
fn a_client_that_stops_reading_ends_the_session() {
    let env = Env::new("client-full");
    let mut cmd = Command::new(BIN);
    let mut child = env
        .command(&mut cmd, &[], &[], "echo", &[])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Hold stdout open but never read it.
    let _stdout = child.stdout.take().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "{}", json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"it","version":"0"}}}))
    .unwrap();
    stdin.flush().unwrap();
    let deadline = Instant::now() + WAIT;
    while env.starts() == 0 {
        assert!(Instant::now() < deadline, "server never started");
        thread::sleep(Duration::from_millis(20));
    }
    // Each gets an error reply; together they're far more than the queue
    // and the pipe hold. mcp-lazy may exit before they're all written.
    for _ in 0..20_000 {
        if writeln!(stdin, "not json").is_err() {
            break;
        }
    }
    let _ = stdin.flush();
    let deadline = Instant::now() + WAIT;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("mcp-lazy kept running with a client that stopped reading");
        }
        thread::sleep(Duration::from_millis(20));
    }
    let server = *env.pids("start ").last().unwrap();
    assert!(wait_dead(server), "server still running");
}

#[test]
fn an_over_long_client_line_gets_an_error_and_the_session_goes_on() {
    let env = Env::new("long-client");
    env.warm("echo");
    let mut s = env.session(&[], "echo");
    s.handshake();
    s.send(json!({"jsonrpc":"2.0","id":90,"method":"ping","params":{"pad":"z".repeat(65 << 20)}}));
    let r = s.recv_where("too-long error", |v| v.get("id") == Some(&Value::Null));
    assert_eq!(r["error"]["code"], -32600, "{r}");
    s.send(json!({"jsonrpc":"2.0","id":91,"method":"ping"}));
    assert_eq!(s.recv_id(json!(91))["result"], json!({}));
    assert!(s.stash.iter().all(|v| v.get("id") != Some(&json!(90))), "the over-long request was answered");
    s.close();
}

#[test]
fn an_over_long_server_line_is_dropped_and_the_session_goes_on() {
    let env = Env::new("long-server");
    env.warm("echo");
    let mut s = env.session(&[], "echo");
    s.handshake();
    assert_eq!(text_of(&s.call(92, "huge_line", json!({}))), "ok");
    assert_eq!(text_of(&s.call(93, "echo", json!({"text":"after"}))), "after");
    assert!(s.stash.iter().all(|v| v["method"] != "notifications/huge"), "the over-long line reached the client");
    s.close();
}
