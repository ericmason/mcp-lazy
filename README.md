# mcp-lazy

A small stdio proxy that keeps a local MCP server stopped until a session calls one of its tools.

Claude Code starts every configured stdio MCP server when a session opens and keeps it running for the whole session, used or not, and Cursor does the same for each window. With many sessions open, that's one copy of every server per session, about 200 MB each for a typical `npx` server. mcp-lazy sits in front of a server and answers the startup handshake and tool listing from a cache, so a session that never calls the server never starts it. An idle mcp-lazy uses about 2 MB.

| 10 idle sessions, 4 servers each | Processes | Memory (RSS) | Session start to tool list |
| --- | --- | --- | --- |
| Direct | 90 | 8,253 MB | about 2 s |
| Through mcp-lazy | 40 | 90 MB | under 0.01 s |

The servers were `@playwright/mcp`, `chrome-devtools-mcp`, `@modelcontextprotocol/server-filesystem`, and `@modelcontextprotocol/server-memory`, each started with `npx` the way MCP configs usually do. The machine was an M1 Max with Node 26. RSS counts shared memory once per process, so the direct total overstates real use somewhat. The startup time moves to the first tool call instead: the first call in a session waits about 2 s while the server starts. To measure your own servers, edit `SERVERS` in `bench/bench.py` and run `python3 bench/bench.py "$(command -v mcp-lazy)" 10 15`. `bench/first_call.py` measures the startup and first-call times.

Codex starts optional stdio servers with a cached tool list lazily by default since [openai/codex PR #62366](https://github.com/openai/codex/pull/62366), so mcp-lazy helps less there.

The design follows [mcp-snooze](https://github.com/askmegit/mcp-snooze) (MIT). This is a separate implementation in Rust, without the config-rewriting commands.

## Install

mcp-lazy runs on macOS and Linux, on Intel and ARM. To install the latest release, run:

```sh
curl -fsSL https://raw.githubusercontent.com/ericmason/mcp-lazy/master/scripts/install.sh | sh
```

You don't need Rust. The script downloads the prebuilt binary for your system from the [latest release](https://github.com/ericmason/mcp-lazy/releases/latest), checks it against the release's `SHA256SUMS`, and installs it to `~/.local/bin/mcp-lazy`. To pin a version or install somewhere else, set `MCP_LAZY_VERSION` (for example, `v0.1.0`) or `MCP_LAZY_INSTALL_DIR` on the `sh` side of the pipe:

```sh
curl -fsSL https://raw.githubusercontent.com/ericmason/mcp-lazy/master/scripts/install.sh | MCP_LAZY_INSTALL_DIR=/usr/local/bin sh
```

To download by hand, pick `mcp-lazy-<version>-<darwin|linux>-<arm64|amd64>.tar.gz` from the releases page. Each archive holds the `mcp-lazy` binary, and the binaries for Linux are static.

If you have Rust, you can build it instead with `cargo install --locked --git https://github.com/ericmason/mcp-lazy`, or with `cargo install --locked --path .` from a checkout.

### Set it up with a coding agent

To have Claude Code, Codex, or Cursor install mcp-lazy and put it in front of your servers, paste this prompt into the agent:

````text
Install mcp-lazy (https://github.com/ericmason/mcp-lazy) and put it in front of my local stdio MCP servers. Read its README first.

1. Install it with `curl -fsSL https://raw.githubusercontent.com/ericmason/mcp-lazy/master/scripts/install.sh | sh`, which installs a prebuilt binary to ~/.local/bin/mcp-lazy. Use that absolute path as the command in every config, because MCP clients often don't have ~/.local/bin on their PATH.
2. Find every MCP server config I have. Check each of these that exists:
   - Claude Code: `claude mcp list`, ~/.claude.json (user scope, plus local scope under "projects"), and .mcp.json in my projects.
   - Codex: [mcp_servers.<name>] tables in ~/.codex/config.toml.
   - Cursor: ~/.cursor/mcp.json and .cursor/mcp.json in my projects.
3. Back up each file before you change it. Never print the values in any "env" block; they often hold tokens.
4. Wrap only stdio servers, the entries with a "command". Skip entries that use a "url" or an HTTP or SSE transport, and skip entries whose command is already mcp-lazy. For each server you wrap, make the command the mcp-lazy path and the arguments the options, then "--", then the original command and its original arguments. Leave "env", "cwd", and every other field as they were.
5. Pick the options for each server:
   - Browser, device, or desktop-automation servers, and anything that keeps sign-ins or open pages in memory: --idle 14400.
   - A server that opens a sign-in page or starts other work on its own at startup: add --discovery-idle 0.
   - Everything else: no options, which gives a 10-minute idle stop.
   Show me the table of servers, the options you chose, and why, before you write any file.
6. After I approve, write the changes. For Claude Code, prefer `claude mcp remove` and `claude mcp add-json` over editing ~/.claude.json by hand.
7. Check each wrapped server: pipe an initialize request and a tools/list request into its new command line, and confirm you get the tool list. Then confirm the cache folder (~/Library/Caches/mcp-lazy on macOS, ~/.cache/mcp-lazy on Linux) has a new file. Run the same command again and confirm it answers from the cache: stderr has no "mcp-lazy: started" line this time, and the cache file's modified time doesn't change.
8. Tell me which servers you wrapped, which you skipped and why, where the backups are, and that running sessions keep their old servers until they restart.
````

## Use

Put `mcp-lazy --` in front of the server's command. In `~/.claude.json`:

```json
"playwright": {
  "command": "mcp-lazy",
  "args": ["--idle", "14400", "--", "playwright-mcp", "--headless"]
}
```

Running sessions keep their old servers until they restart.

| Option | Default | What it does |
| --- | --- | --- |
| `--idle SECONDS` | 600 | Stop the server after this long with nothing in flight. 0 never stops it. |
| `--discovery-idle SECONDS` | 10 | Stop a server that has only answered `initialize` and list requests after this long, whatever `--idle` says. It holds nothing the session made yet. 0 uses `--idle` instead. |
| `--max-age SECONDS` | 604800 (7 days) | Ignore cached replies older than this, and start the server to refresh them. 0 never expires them. |
| `--start-timeout SECONDS` | 60 | Fail waiting requests if the server doesn't answer `initialize` in time. |
| `--cache-dir DIR` | `MCP_LAZY_CACHE_DIR`, else `~/Library/Caches/mcp-lazy` (macOS) or `~/.cache/mcp-lazy` | Where the cache lives. |
| `--ignore-env PATTERN` | none | Leave environment variables matching this name out of the cache key. `*` matches any text. Repeat for more patterns. Use it for a variable that changes every session but doesn't change the server's tools. |

`mcp-lazy clear-cache` deletes every cache file.

### Choosing `--idle`

Stopping a server loses everything it holds in memory: open browser tabs, sign-ins, subscriptions. For browser and device servers (playwright-mcp, chrome-devtools-mcp), use hours, or 0.

`--discovery-idle` stops a server that no request has used yet, 10 seconds after discovery by default. If a server does something on its own at startup that the user has to finish, such as opening a sign-in page, set `--discovery-idle 0` so it waits for `--idle` instead. For servers that only answer requests, the default is fine.

## How it works

1. **First session for a server, no cache yet:** mcp-lazy starts the server and passes everything through, saving the server's `initialize` result and the first page of `tools/list`, `prompts/list`, `resources/list`, and `resources/templates/list`. If the session calls nothing, mcp-lazy stops the server after `--discovery-idle` seconds instead of `--idle`.
2. **Later sessions:** mcp-lazy answers `initialize`, those list requests, `ping`, and `logging/setLevel` itself. No server runs.
3. **First other request** (such as `tools/call`): mcp-lazy starts the server, replays the session's own `initialize` and `notifications/initialized`, then sends the request. Requests that arrive while it starts wait in order.
4. **After starting,** mcp-lazy asks the server for its lists again. If one differs from the cache, it updates the cache and sends the matching `list_changed` notification, so the session picks up new tools.
5. **Idle:** after `--idle` seconds with no request in flight in either direction, mcp-lazy stops the server's whole process group (SIGTERM, then SIGKILL after 2 seconds), so `npx` and `uvx` children stop too. The next request starts it again.
6. **Session ends** (stdin closes): mcp-lazy waits up to 2 seconds for answers still in flight, then stops the server.
7. **mcp-lazy gets SIGTERM, SIGINT, or SIGHUP:** it stops the server's process group the same way, then exits with status 128 plus the signal number.

If the server exits or fails to start, every request waiting on it gets a JSON-RPC error (`-32603`), and the next request starts it again. mcp-lazy watches the server process itself, so a crash is noticed even when something the server started still holds its output open.

### The cache

- There's one file per server command, working directory, environment, and protocol version. Two MCP configs that differ only in their `env` block get separate entries.
- Each file holds the server's `initialize` result, the first page of each list method, the time it was saved, and a SHA-256 digest of the key. It never holds the command, its arguments, the working directory, or any environment value, so a token passed as an argument (`--api-key=…`) or in a connection string doesn't reach the disk. The file name is the first 32 hex digits of the same digest. mcp-lazy uses a file only when its stored digest matches.
- Variables that change in every session or terminal are left out of the key, such as every `CLAUDE_CODE_*` and `CODELLO_BRIDGE_*` variable, `CLAUDE_PROJECT_DIR`, `TERM_SESSION_ID`, socket paths (names ending in `_SOCK` or `_SOCKET`), and other names containing `SESSION` or ending in `_PID`. The full list is `VOLATILE_ENV` in `src/cache.rs`. If another client sets a per-session variable that isn't on the list, every session misses the cache; leave it out with `--ignore-env`.
- Each time the server confirms its lists, the entry's age resets, even when nothing changed, so `--max-age` counts from the last check.
- Files are mode 600 in a mode 700 directory. mcp-lazy ignores a cache file that is a symlink, isn't a regular file, or is owned by another user or writable by others, and a directory owned by another user or writable by others.
- Writes are atomic, so concurrent sessions can share the cache.
- `clear-cache` deletes only files with the names mcp-lazy gives cache entries, and refuses a directory that fails the ownership check, so pointing `--cache-dir` at another folder by mistake doesn't delete its files.
- If neither `HOME` nor the user database gives a home directory, and you set no cache directory, mcp-lazy caches nothing and starts the server for every session.

### Limits

- stdio servers only. HTTP and SSE servers already run once and need no proxy.
- A paginated list request (one with a `cursor`) always goes to the server.
- JSON-RPC batches get an error (`-32600`); MCP dropped batching in the 2025-06-18 revision. A line that isn't JSON gets a parse error (`-32700`). Neither starts the server.
- A message can be at most 64 MiB, not counting its line ending. A longer message from the client gets an error (`-32600`) with a `null` id, because mcp-lazy doesn't read far enough to find the request's id; the client's own timeout ends that request. A longer message from the server is dropped with a note on stderr; if it was an answer, the request it answered waits until the client's timeout or the server stops. Either way, the messages after it still go through.
- mcp-lazy holds up to 1,024 messages for a reader that isn't keeping up, in each direction. If the server stops reading its input and 1,024 messages are already waiting for it, each new request gets an error (`-32603`) and other messages are dropped, with a note on stderr. If the client stops reading and 1,024 messages are already waiting for it, mcp-lazy treats the client as gone: it stops the server and exits.
- If mcp-lazy starts with SIGHUP or SIGINT ignored, as under `nohup` or in a background job, that signal stays ignored. Otherwise, SIGTERM, SIGINT, and SIGHUP stop the server and mcp-lazy.
- The command mcp-lazy runs must stay running as the server. A wrapper script that starts the server in the background and exits counts as a crash; end the wrapper with `exec`.
- The 2026-07-28 MCP revision drops `initialize`. mcp-lazy answers `server/discover` with "Method not found" so clients that speak both revisions fall back to `initialize`. Clients that speak only the new revision aren't supported yet.
- Unix only (macOS and Linux).

## Develop

```sh
cargo test   # unit tests, plus end-to-end tests against tests/fixtures/fake_server.py (needs python3)
cargo build --release
```

To release, run `scripts/release.sh patch` (or `minor`, `major`, or an exact version) on master. It sets the version in `Cargo.toml`, runs the tests, and pushes a `v*` tag. The tag starts `.github/workflows/release.yml`, which builds the four binaries, creates the GitHub release, and uploads the archives and `SHA256SUMS`.

## Security

Report a vulnerability privately through [GitHub security advisories](https://github.com/ericmason/mcp-lazy/security/advisories/new), not in a public issue.

## License

MIT. See [LICENSE](LICENSE).
