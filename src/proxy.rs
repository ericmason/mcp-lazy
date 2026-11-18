//! The event loop between the client (our stdin/stdout) and the real server.
//!
//! Invariants:
//! - Every client request gets exactly one response: the server's, a cached
//!   one, or an error from the proxy. None are dropped or duplicated.
//! - The server is stopped for idling only when no client request or server
//!   request is in flight and nothing is queued. The proxy's own refreshes
//!   don't hold it up: losing one only means the cache isn't refreshed.
//!   A server that has only answered discovery holds nothing the session
//!   made, so it waits --discovery-idle, not --idle, once its refreshes are back.
//! - Messages are forwarded as the original bytes; only proxy-made messages
//!   are built here.
//! - The cache only ever stores initialize results and first pages of list
//!   methods.
//! - Messages from a stopped server (an older generation) are ignored.
//! - The event loop never blocks on a write: the server's stdin and our
//!   stdout each have a writer thread with a bounded queue, so timers,
//!   signals, and shutdown always run. When the server's queue is full, the
//!   message gets an error (a request) or is dropped (anything else); when
//!   ours is full, the client is treated as gone.

use crate::cache::{Cache, Key, LIST_METHODS};
use crate::child::{read_lines, Child, Line, MAX_LINE};
use crate::msg::{self, Msg};
use crate::sys;
use crate::writer::{SendError, Writer, QUEUE_LINES};
use crate::Config;
use serde_json::value::RawValue;
use std::collections::{HashMap, HashSet};
use std::io::{self, Read};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

pub enum Event {
    Client(Vec<u8>),
    /// The client sent a line longer than `MAX_LINE`, which was dropped.
    ClientTooLong,
    ClientEof,
    /// A write to our stdout failed.
    ClientOutputClosed,
    Server(u64, Vec<u8>),
    ServerEof(u64),
    ServerExit(u64),
    /// A write to the server's stdin failed.
    ServerInputClosed(u64),
    /// mcp-lazy got SIGTERM, SIGINT, or SIGHUP.
    Signal(i32),
}

const EOF_GRACE: Duration = Duration::from_secs(2);
/// How long after the server process exits to keep reading what it already
/// wrote, before treating it as gone.
const EXIT_GRACE: Duration = Duration::from_millis(300);
const PARSE_ERROR: i64 = -32700;
const SERVER_ERROR: i64 = -32603;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_REQUEST: i64 = -32600;

enum State {
    Down,
    /// Spawned; waiting for the reply to the replayed initialize.
    Starting(Child),
    Up(Child),
}

enum ProxyReq {
    Init,
    Refresh { method: &'static str, notify: bool },
    Ignore,
}

struct Pending {
    id: String,
    /// The method whose result goes in the cache, when this request is cacheable.
    capture: Option<String>,
}

struct Proxy {
    cfg: Config,
    cwd: String,
    env: String,
    tx: Sender<Event>,
    /// Our stdout, to the client.
    out: Writer,
    cache: Option<Cache>,
    client_init_params: Option<String>,
    client_initialized: bool,
    /// True when the client's own initialize went to this server process, so
    /// its notifications/initialized must be forwarded rather than replayed.
    client_did_handshake: bool,
    log_level_params: Option<String>,
    state: State,
    gen: u64,
    pending: HashMap<String, Pending>,
    queue: Vec<Vec<u8>>,
    server_reqs: HashSet<String>,
    proxy_reqs: HashMap<String, ProxyReq>,
    next_proxy_id: u64,
    /// True once this server process got a request other than initialize,
    /// a first-page list, ping, or logging/setLevel.
    used: bool,
    last_activity: Instant,
    start_deadline: Option<Instant>,
    /// Set when the server process exited but its stdout is still open.
    exit_deadline: Option<Instant>,
    shutdown_deadline: Option<Instant>,
}

pub fn run(cfg: Config) -> i32 {
    sys::reset_sigchld();
    let (tx, rx) = mpsc::channel();
    if let Err(e) = catch_signals(tx.clone()) {
        note!("mcp-lazy: could not catch SIGTERM, SIGINT, and SIGHUP: {e}");
    }
    let client_tx = tx.clone();
    let reader = thread::Builder::new().name("client-stdin".into()).stack_size(256 * 1024).spawn(move || {
        read_lines(io::stdin(), MAX_LINE, |l| match l {
            Line::Data(l) => client_tx.send(Event::Client(l)).is_ok(),
            Line::TooLong => client_tx.send(Event::ClientTooLong).is_ok(),
        });
        let _ = client_tx.send(Event::ClientEof);
    });
    if let Err(e) = reader {
        note!("mcp-lazy: {e}");
        return 1;
    }
    let out_tx = tx.clone();
    let out = match Writer::spawn("client-stdout", io::stdout(), move || {
        let _ = out_tx.send(Event::ClientOutputClosed);
    }) {
        Ok(w) => w,
        Err(e) => {
            note!("mcp-lazy: {e}");
            return 1;
        }
    };
    let cwd = std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let vars = std::env::vars_os().map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned()));
    let env = crate::cache::env_digest(vars, &cfg.ignore_env);
    let mut p = Proxy {
        cfg,
        cwd,
        env,
        tx,
        out,
        cache: None,
        client_init_params: None,
        client_initialized: false,
        client_did_handshake: false,
        log_level_params: None,
        state: State::Down,
        gen: 0,
        pending: HashMap::new(),
        queue: Vec::new(),
        server_reqs: HashSet::new(),
        proxy_reqs: HashMap::new(),
        next_proxy_id: 0,
        used: false,
        last_activity: Instant::now(),
        start_deadline: None,
        exit_deadline: None,
        shutdown_deadline: None,
    };
    let code = loop {
        if p.done() {
            break 0;
        }
        let ev = match p.next_deadline() {
            None => match rx.recv() {
                Ok(ev) => ev,
                Err(_) => break 0,
            },
            Some(d) => match rx.recv_timeout(d.saturating_duration_since(Instant::now())) {
                Ok(ev) => ev,
                Err(RecvTimeoutError::Timeout) => {
                    p.on_timer();
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break 0,
            },
        };
        match ev {
            Event::Client(line) => p.on_client(line),
            Event::ClientTooLong => p.on_client_too_long(),
            Event::ClientEof => p.shutdown_deadline = Some(Instant::now() + EOF_GRACE),
            Event::ClientOutputClosed => p.client_gone(),
            Event::Server(gen, line) => p.on_server(gen, line),
            Event::ServerEof(gen) => p.on_server_eof(gen),
            // Either way, keep reading what the server already wrote for a
            // moment before failing what still waits on it.
            Event::ServerExit(gen) | Event::ServerInputClosed(gen) => {
                if gen == p.gen && !matches!(p.state, State::Down) && p.exit_deadline.is_none() {
                    p.exit_deadline = Some(Instant::now() + EXIT_GRACE);
                }
            }
            Event::Signal(sig) => {
                note!("mcp-lazy: got signal {sig}, stopping");
                break 128 + sig;
            }
        }
    };
    p.stop_server();
    // Give the client what's still queued for it, but don't wait on a
    // client that stopped reading.
    p.out.finish(EOF_GRACE);
    code
}

/// Turns each caught stop signal into an `Event::Signal`.
fn catch_signals(tx: Sender<Event>) -> io::Result<()> {
    let mut sock = sys::catch_stop_signals()?;
    thread::Builder::new().name("signals".into()).stack_size(64 * 1024).spawn(move || {
        let mut b = [0u8; 1];
        loop {
            match sock.read(&mut b) {
                Ok(1) => {
                    if tx.send(Event::Signal(b[0] as i32)).is_err() {
                        return;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                _ => return,
            }
        }
    })?;
    Ok(())
}

impl Proxy {
    fn is_up(&self) -> bool {
        matches!(self.state, State::Up(_))
    }

    fn done(&self) -> bool {
        match self.shutdown_deadline {
            None => false,
            Some(d) => (self.pending.is_empty() && self.queue.is_empty()) || Instant::now() >= d,
        }
    }

    fn idle_deadline(&self) -> Option<Instant> {
        let idle = self.cfg.idle?;
        let quiet = self.is_up()
            && self.pending.is_empty()
            && self.server_reqs.is_empty()
            && self.queue.is_empty();
        // A server nobody has used yet stops soon after discovery, once its
        // lists are refreshed. If a refresh never comes back, --idle applies.
        let wait = match self.cfg.discovery_idle {
            Some(d) if !self.used && self.proxy_reqs.is_empty() => idle.min(d),
            _ => idle,
        };
        quiet.then(|| self.last_activity + wait)
    }

    fn next_deadline(&self) -> Option<Instant> {
        [self.start_deadline, self.exit_deadline, self.idle_deadline(), self.shutdown_deadline]
            .into_iter()
            .flatten()
            .min()
    }

    fn on_timer(&mut self) {
        let now = Instant::now();
        if self.exit_deadline.is_some_and(|d| now >= d) {
            // Exited, but something it started still holds its stdout open.
            self.on_server_eof(self.gen);
        }
        if matches!(self.state, State::Starting(_)) && self.start_deadline.is_some_and(|d| now >= d) {
            self.fail_start("timed out waiting for the MCP server to answer initialize");
        }
        if self.idle_deadline().is_some_and(|d| now >= d) {
            note!("mcp-lazy: stopping idle server");
            self.stop_server();
        }
    }

    // ---- client -> proxy ----

    fn send_client(&mut self, line: &[u8]) {
        match self.out.send(line) {
            Ok(()) => {}
            Err(SendError::Full) => {
                if self.shutdown_deadline.is_none() {
                    note!("mcp-lazy: the client stopped reading ({QUEUE_LINES} messages waiting); ending the session");
                }
                self.client_gone();
            }
            Err(SendError::Closed) => self.client_gone(),
        }
    }

    /// Nobody is reading our output; finish now.
    fn client_gone(&mut self) {
        self.shutdown_deadline = Some(Instant::now());
        self.pending.clear();
        self.queue.clear();
    }

    fn reply(&mut self, id: &str, result: &str) {
        self.send_client(&msg::response(id, result));
    }

    fn reply_error(&mut self, id: &str, code: i64, message: &str) {
        self.send_client(&msg::error(id, code, message));
    }

    fn on_client(&mut self, line: Vec<u8>) {
        if self.shutdown_deadline.is_some() {
            return;
        }
        self.last_activity = Instant::now();
        let Some(m) = msg::parse(&line) else {
            // MCP dropped batches in 2025-06-18, and an untracked request could
            // go unanswered, so neither a batch nor garbage goes to the server.
            if msg::is_batch(&line) {
                self.reply_error("null", INVALID_REQUEST, "mcp-lazy: JSON-RPC batches are not supported");
            } else {
                self.reply_error("null", PARSE_ERROR, "mcp-lazy: not a JSON-RPC message");
            }
            return;
        };
        match (&m.method, m.id) {
            (Some(_), Some(id)) if msg::is_proxy_id(id) => {
                // The proxy's own requests use these ids, so the reply would be taken for one of them.
                let id = id.get().to_string();
                self.reply_error(&id, INVALID_REQUEST, "mcp-lazy: request ids starting with \"mcp-lazy:\" are reserved")
            }
            (Some(method), Some(id)) => {
                let method = method.clone();
                self.client_request(&line, &m, id, &method)
            }
            (Some(method), None) => {
                let method = method.clone();
                self.client_notification(&line, &m, &method)
            }
            (None, Some(id)) => {
                // A reply to a request the server sent.
                let key = msg::id_key(id);
                if !matches!(self.state, State::Down) && self.server_reqs.remove(&key) {
                    self.write_server(&line);
                }
            }
            (None, None) => {}
        }
    }

    /// The line was too long to read, so its id is unknown. Answer with a
    /// null id, as for a line that isn't JSON; the session goes on.
    fn on_client_too_long(&mut self) {
        if self.shutdown_deadline.is_some() {
            return;
        }
        note!("mcp-lazy: dropped a line from the client longer than {} MiB", MAX_LINE >> 20);
        let message = format!("mcp-lazy: message longer than {} MiB", MAX_LINE >> 20);
        self.reply_error("null", INVALID_REQUEST, &message);
    }

    fn client_request(&mut self, line: &[u8], m: &Msg, id: &RawValue, method: &str) {
        let id_raw = id.get().to_string();
        let down = !self.is_up();
        match method {
            "initialize" => {
                self.client_init_params = Some(m.params.map(|p| p.get().to_string()).unwrap_or_else(|| "{}".into()));
                let key = Key {
                    argv: self.cfg.argv.clone(),
                    cwd: self.cwd.clone(),
                    env: self.env.clone(),
                    protocol_version: msg::protocol_version(m.params),
                };
                let mut hit = false;
                self.cache = self.cfg.cache_dir.as_deref().map(|dir| {
                    let (cache, h) = Cache::load(dir, key, self.cfg.max_age);
                    hit = h;
                    cache
                });
                if hit && matches!(self.state, State::Down) {
                    let init = self.cache.as_ref().and_then(|c| c.initialize.as_ref()).map(|r| r.get().to_string());
                    if let Some(init) = init {
                        self.reply(&id_raw, &init);
                        return;
                    }
                }
                // No cache: start the server now and let the client do its own handshake.
                if matches!(self.state, State::Down) {
                    if let Err(e) = self.spawn_server() {
                        self.reply_error(&id_raw, SERVER_ERROR, &format!("mcp-lazy: could not start the MCP server: {e}"));
                        return;
                    }
                    self.client_did_handshake = true;
                    self.state = match std::mem::replace(&mut self.state, State::Down) {
                        State::Starting(c) => State::Up(c),
                        other => other,
                    };
                }
                self.forward_request(line, id, method, m.params);
            }
            "ping" if down => self.reply(&id_raw, "{}"),
            "logging/setLevel" => {
                self.log_level_params = m.params.map(|p| p.get().to_string());
                if down {
                    self.reply(&id_raw, "{}");
                } else {
                    self.forward_request(line, id, method, m.params);
                }
            }
            // Clients that speak both the 2026-07-28 revision and older ones fall
            // back to initialize when discover fails.
            "server/discover" if down && self.client_init_params.is_none() => {
                self.reply_error(&id_raw, METHOD_NOT_FOUND, "Method not found")
            }
            _ => {
                if down && LIST_METHODS.contains(&method) && !msg::has_cursor(m.params) {
                    let cached = self.cache.as_ref().and_then(|c| c.lists.get(method)).map(|r| r.get().to_string());
                    if let Some(r) = cached {
                        self.reply(&id_raw, &r);
                        return;
                    }
                }
                match self.state {
                    State::Up(_) => self.forward_request(line, id, method, m.params),
                    State::Starting(_) => self.queue.push(line.to_vec()),
                    State::Down => match self.start_replay() {
                        Ok(()) => self.send_or_queue(line.to_vec()),
                        Err(e) => {
                            self.reply_error(&id_raw, SERVER_ERROR, &format!("mcp-lazy: could not start the MCP server: {e}"))
                        }
                    },
                }
            }
        }
    }

    fn client_notification(&mut self, line: &[u8], m: &Msg, method: &str) {
        if method == "notifications/cancelled" && msg::cancelled_request_id(m.params).is_some_and(msg::is_proxy_id) {
            // Names one of the proxy's own requests, which the client never sent.
            return;
        }
        match method {
            "notifications/initialized" => {
                self.client_initialized = true;
                if self.client_did_handshake && self.is_up() {
                    self.write_server(line);
                }
            }
            "notifications/cancelled" => match self.state {
                State::Up(_) => {
                    // A server that honors the cancel never replies, so stop waiting
                    // for one. A late reply still reaches the client.
                    if let Some(t) = msg::cancelled_request_key(m.params) {
                        self.pending.remove(&t);
                    }
                    self.write_server(line);
                }
                State::Starting(_) => {
                    let target = msg::cancelled_request_key(m.params);
                    let before = self.queue.len();
                    if let Some(t) = target {
                        self.queue.retain(|q| msg::parse(q).and_then(|qm| qm.id.map(msg::id_key)).as_ref() != Some(&t));
                    }
                    if self.queue.len() == before {
                        self.queue.push(line.to_vec());
                    }
                }
                State::Down => {}
            },
            _ => match self.state {
                State::Up(_) => {
                    self.write_server(line);
                }
                State::Starting(_) => self.queue.push(line.to_vec()),
                State::Down => {}
            },
        }
    }

    /// After a start: queue behind the replayed initialize, or send now when
    /// there was nothing to replay and the server is already up.
    fn send_or_queue(&mut self, line: Vec<u8>) {
        if self.is_up() {
            self.forward_queued(line);
        } else {
            self.queue.push(line);
        }
    }

    /// Sends a client request to the running server and tracks it until answered.
    fn forward_request(&mut self, line: &[u8], id: &RawValue, method: &str, params: Option<&RawValue>) {
        let capture = (method == "initialize" || (LIST_METHODS.contains(&method) && !msg::has_cursor(params)))
            .then(|| method.to_string());
        if capture.is_none() && method != "ping" && method != "logging/setLevel" {
            self.used = true;
        }
        let key = msg::id_key(id);
        self.pending.insert(key.clone(), Pending { id: id.get().to_string(), capture });
        if !self.write_server(line) {
            self.pending.remove(&key);
            self.reply_error(id.get(), SERVER_ERROR, "mcp-lazy: could not write to the MCP server");
        }
    }

    /// Replays a queued message once the server is up.
    fn forward_queued(&mut self, line: Vec<u8>) {
        let Some(m) = msg::parse(&line) else {
            self.write_server(&line);
            return;
        };
        match (&m.method, m.id) {
            (Some(method), Some(id)) => {
                let method = method.clone();
                self.forward_request(&line, id, &method, m.params)
            }
            _ => {
                self.write_server(&line);
            }
        }
    }

    // ---- server lifecycle ----

    fn spawn_server(&mut self) -> io::Result<()> {
        self.gen += 1;
        let child = Child::spawn(&self.cfg.argv, self.gen, self.tx.clone())?;
        note!("mcp-lazy: started {}", self.cfg.argv[0]);
        self.state = State::Starting(child);
        self.client_did_handshake = false;
        Ok(())
    }

    /// Starts the server and replays the client's initialize to it.
    fn start_replay(&mut self) -> io::Result<()> {
        self.spawn_server()?;
        match self.client_init_params.clone() {
            Some(params) => {
                let id = self.new_proxy_req(ProxyReq::Init);
                if !self.write_server(&msg::request(&id, "initialize", Some(&params))) {
                    self.stop_server();
                    return Err(io::Error::new(io::ErrorKind::BrokenPipe, "the MCP server closed its input"));
                }
                self.start_deadline = Some(Instant::now() + self.cfg.start_timeout);
            }
            None => {
                // The client never initialized; pass everything through.
                self.client_did_handshake = true;
                if let State::Starting(c) = std::mem::replace(&mut self.state, State::Down) {
                    self.state = State::Up(c);
                }
            }
        }
        Ok(())
    }

    fn new_proxy_req(&mut self, r: ProxyReq) -> String {
        self.next_proxy_id += 1;
        let id = serde_json::to_string(&format!("{}{}", msg::PROXY_ID_PREFIX, self.next_proxy_id)).unwrap();
        self.proxy_reqs.insert(id.clone(), r);
        id
    }

    /// Queues a line for the server. False if it wasn't queued: the server is
    /// down, its stdin closed, or it stopped reading and its queue is full.
    fn write_server(&mut self, line: &[u8]) -> bool {
        match &mut self.state {
            State::Up(c) | State::Starting(c) => match c.write(line) {
                Ok(()) => true,
                Err(SendError::Full) => {
                    note!("mcp-lazy: the MCP server isn't reading its input ({QUEUE_LINES} messages waiting); dropped a message");
                    false
                }
                Err(SendError::Closed) => false,
            },
            State::Down => false,
        }
    }

    fn stop_server(&mut self) {
        if let State::Up(c) | State::Starting(c) = std::mem::replace(&mut self.state, State::Down) {
            c.stop();
        }
        self.start_deadline = None;
        self.exit_deadline = None;
        self.proxy_reqs.clear();
        self.server_reqs.clear();
        self.client_did_handshake = false;
        self.used = false;
    }

    /// Answers every client request that was waiting on the server with an error.
    fn fail_waiting(&mut self, message: &str) {
        let pending: Vec<Pending> = self.pending.drain().map(|(_, p)| p).collect();
        for p in pending {
            self.reply_error(&p.id, SERVER_ERROR, message);
        }
        for line in std::mem::take(&mut self.queue) {
            if let Some(m) = msg::parse(&line) {
                if let (Some(_), Some(id)) = (&m.method, m.id) {
                    let id = id.get().to_string();
                    self.reply_error(&id, SERVER_ERROR, message);
                }
            }
        }
    }

    fn fail_start(&mut self, reason: &str) {
        note!("mcp-lazy: {reason}");
        self.stop_server();
        self.fail_waiting(&format!("mcp-lazy: {reason}"));
    }

    // ---- server -> proxy ----

    fn on_server(&mut self, gen: u64, line: Vec<u8>) {
        if gen != self.gen || matches!(self.state, State::Down) {
            return;
        }
        // Notifications (progress, logs, heartbeats) don't count as activity,
        // or a chatty server would never be stopped for idling.
        let Some(m) = msg::parse(&line) else {
            self.send_client(&line);
            return;
        };
        match (&m.method, m.id) {
            (None, Some(id)) => {
                let key = msg::id_key(id);
                if let Some(r) = self.proxy_reqs.remove(&key) {
                    // The proxy's own requests aren't activity either.
                    self.on_proxy_reply(r, m.result, m.error.is_some());
                    return;
                }
                self.last_activity = Instant::now();
                if let Some(p) = self.pending.remove(&key) {
                    if let (Some(method), Some(result), None) = (p.capture, m.result, m.error) {
                        self.capture(&method, result);
                    }
                }
                self.send_client(&line);
            }
            (Some(_), Some(id)) => {
                self.last_activity = Instant::now();
                self.server_reqs.insert(msg::id_key(id));
                self.send_client(&line);
            }
            (Some(method), None) => {
                if method == "notifications/cancelled" {
                    // The server gave up on its own request; the client won't answer it.
                    if let Some(t) = msg::cancelled_request_key(m.params) {
                        self.server_reqs.remove(&t);
                    }
                }
                let lists = msg::lists_for_changed(method);
                if self.is_up() {
                    for &list in lists {
                        if self.cache.as_ref().is_some_and(|c| c.lists.contains_key(list)) {
                            let id = self.new_proxy_req(ProxyReq::Refresh { method: list, notify: false });
                            self.write_server(&msg::request(&id, list, None));
                        }
                    }
                }
                self.send_client(&line);
            }
            (None, None) => self.send_client(&line),
        }
    }

    fn on_proxy_reply(&mut self, r: ProxyReq, result: Option<&RawValue>, is_error: bool) {
        match r {
            ProxyReq::Init => {
                let Some(result) = result.filter(|_| !is_error) else {
                    self.fail_start("the MCP server rejected the replayed initialize");
                    return;
                };
                self.start_deadline = None;
                if let State::Starting(c) = std::mem::replace(&mut self.state, State::Down) {
                    self.state = State::Up(c);
                }
                self.capture("initialize", result);
                self.write_server(&msg::notification("notifications/initialized"));
                if let Some(p) = self.log_level_params.clone() {
                    let id = self.new_proxy_req(ProxyReq::Ignore);
                    self.write_server(&msg::request(&id, "logging/setLevel", Some(&p)));
                }
                for line in std::mem::take(&mut self.queue) {
                    self.forward_queued(line);
                }
                for list in msg::advertised_lists(result) {
                    let id = self.new_proxy_req(ProxyReq::Refresh { method: list, notify: true });
                    self.write_server(&msg::request(&id, list, None));
                }
            }
            ProxyReq::Refresh { method, notify } => {
                if let (Some(result), false) = (result, is_error) {
                    if self.capture(method, result) && notify && self.client_initialized {
                        if let Some(n) = msg::list_changed_for(method) {
                            self.send_client(&msg::notification(n));
                        }
                    }
                }
            }
            ProxyReq::Ignore => {}
        }
    }

    /// Stores a discovery result and saves the cache even when nothing
    /// changed, so --max-age counts from the last time the server confirmed
    /// it. Returns true if the cached copy changed.
    fn capture(&mut self, method: &str, result: &RawValue) -> bool {
        let Some(cache) = self.cache.as_mut() else { return false };
        let changed = if method == "initialize" { cache.set_initialize(result) } else { cache.set_list(method, result) };
        if let Err(e) = cache.save() {
            note!("mcp-lazy: could not write the cache: {e}");
        }
        changed
    }

    fn on_server_eof(&mut self, gen: u64) {
        if gen != self.gen || matches!(self.state, State::Down) {
            return;
        }
        let starting = matches!(self.state, State::Starting(_));
        note!("mcp-lazy: the MCP server exited");
        self.stop_server();
        let why = if starting {
            "mcp-lazy: the MCP server exited while starting"
        } else {
            "mcp-lazy: the MCP server exited before answering"
        };
        self.fail_waiting(why);
    }
}
