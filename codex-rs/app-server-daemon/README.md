# codex-app-server-daemon

> `codex-app-server-daemon` is experimental and its lifecycle contract may
> change while the remote-management flow is still being developed.

`codex-app-server-daemon` backs the machine-readable `codex app-server`
lifecycle commands used by remote clients such as the desktop and mobile apps.
It is intended for Codex instances launched over SSH, including fresh developer
machines that should expose app-server with `remote_control` enabled.

## Platform support

The daemon supports Linux, macOS, and Windows using platform-specific process
and file-locking primitives. Windows startup requires a non-elevated terminal
whose host permits detached child processes.

Windows automatic attachment requires the canonical socket address to fit the
108-byte AF_UNIX limit (including its terminator). A short junction alias whose
resolved address exceeds that limit falls back to the embedded server. Use a
shorter `CODEX_HOME` to share the daemon; discovery does not trust a mutable alias.

Shared clients use the environment inherited when the daemon started. Opening a
new terminal or clearing variables there does not clear the running daemon's
environment; per-client environment isolation is not provided.
An invocation that sets `CODEX_EXEC_SERVER_URL` skips implicit daemon attachment
so its executor selection is preserved. If an implicitly discovered daemon cannot
initialize the connection, the TUI starts an embedded server instead. Explicit
`--remote` endpoints remain authoritative and report connection failures.

## Commands

```sh
codex app-server daemon start
codex app-server daemon restart
codex app-server daemon enable-remote-control
codex app-server daemon disable-remote-control
codex app-server daemon stop
codex app-server daemon version
codex app-server daemon bootstrap --remote-control
```

On success, every command writes exactly one JSON object to stdout. Consumers
should parse that JSON rather than relying on human-readable text. Lifecycle
responses report the resolved backend, socket path, local CLI version, and
running app-server version when applicable.

## Bootstrap flow

For a new Linux or macOS machine:

```sh
curl -fsSL https://github.com/yuguorui/codex/releases/latest/download/install-fork.sh | sh
$HOME/.codex/packages/standalone/current/bin/codex++ app-server daemon bootstrap --remote-control
```

On Windows, use a non-elevated PowerShell terminal whose host allows breakaway:

```powershell
irm https://chatgpt.com/codex/install.ps1 | iex
$codexHome = if ($env:CODEX_HOME) { $env:CODEX_HOME } else { Join-Path $HOME '.codex' }
& "$codexHome\packages\standalone\current\bin\codex.exe" app-server daemon bootstrap --remote-control
```

`bootstrap` requires the standalone managed install. It records the daemon
settings under `CODEX_HOME/app-server-daemon/`, starts app-server as a
pidfile-backed detached process, and launches a detached updater loop.

## Installation and update cases

The daemon assumes Codex++ is installed through `install-fork.sh` and always launches
the standalone managed binary under `CODEX_HOME`.

| Situation | What starts | Does this daemon fetch new binaries? | Does a running app-server eventually move to a newer binary on its own? |
| --- | --- | --- | --- |
| `install-fork.sh` has run, but only `start` is used | `start` uses `CODEX_HOME/packages/standalone/current/bin/codex++` | No | No. The managed path is used when starting or restarting, but no updater is installed. |
| `install-fork.sh` has run, then `bootstrap` is used | The pidfile backend uses `CODEX_HOME/packages/standalone/current/bin/codex++` | Yes. Bootstrap launches a detached updater loop that runs `install-fork.sh` hourly. | Yes, while that updater process is alive and app-server is already running. After a successful fetch, the updater restarts app-server with the refreshed binary and only then replaces its own process image. |
| Some other tool updates the managed binary path | The next fresh start or restart uses the updated file at that path | Only if `bootstrap` is active, because the updater still runs `install-fork.sh` on its normal cadence. | Without `bootstrap`, no. With `bootstrap`, the next successful updater pass compares the managed binary contents after `install-fork.sh` runs; if app-server is running and they differ from the updater's current image, it refreshes app-server first and then itself. |

### Standalone installs

For installs created by `install-fork.sh`:

- lifecycle commands always use the standalone managed binary path
- `bootstrap` is supported
- `bootstrap` starts a detached pid-backed updater loop that fetches via
  `install-fork.sh`
- after a successful refresh, if app-server is running and the managed binary
  contents changed, the updater restarts app-server with that binary first and
  only then replaces its own process image
- the updater loop is not reboot-persistent; it must be started again by
  rerunning `bootstrap` after a reboot

### Out-of-band updates

This daemon does not watch arbitrary executable files for replacement. If some
other tool updates the managed binary path:

- without `bootstrap`, a currently running app-server remains on the old
  executable image until an explicit `restart`
- with `bootstrap`, the detached updater loop notices the changed managed
  binary on its next successful scheduled pass after running `install-fork.sh`; if
  app-server is running, it refreshes app-server first and then refreshes itself
  once that replacement starts successfully

## Lifecycle semantics

`start` is idempotent and returns after app-server is ready to answer the normal
JSON-RPC initialize handshake on the Unix control socket.

`restart` stops any managed daemon and starts it again.

`enable-remote-control` and `disable-remote-control` persist the launch setting
for future starts. If a managed app-server is already running, they restart it
so the new setting takes effect immediately.

Top-level `codex remote-control` bootstraps with `--remote-control` when the
updater loop is not running. Otherwise it enables remote control and starts the
daemon normally.

`stop` sends a graceful termination request first, then sends a second
termination signal after the grace window if the process is still alive.

All mutating lifecycle commands are serialized per `CODEX_HOME`, so a concurrent
`start`, `restart`, `enable-remote-control`, `disable-remote-control`, `stop`,
or `bootstrap` does not race another in-flight lifecycle operation.

## State

The daemon stores its local state under `CODEX_HOME/app-server-daemon/`:

- `settings.json` for persisted launch settings
- `app-server.pid` for the app-server process record
- `app-server-updater.pid` for the pid-backed standalone updater loop
- `daemon.lock` for daemon-wide lifecycle serialization
