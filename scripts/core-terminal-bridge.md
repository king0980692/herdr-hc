# Core terminal bridge (experimental PoC)

This Unix-only bridge attaches one existing herdr-core terminal to a disposable
Herdr pane. It calls the core framed API directly through `hc connect`; it does
not run `hc attach` or replace either server. No extra Python packages are needed.

Core owns the program and terminal lifetime. The local Herdr server's existing
PTY/parser renders the terminal bytes and draws its own pane layout and borders.
This proves that a core-backed terminal can use local pane styling without a
matching Herdr build on the core machine. It does **not** migrate existing Herdr
remote-machine entries to core.

## Try it

Create a terminal with `hc run --bg`, then use the returned address in a **new,
disposable pane**:

```sh
python3 /path/to/herdr/scripts/core_terminal_bridge.py t1
```

Select a remote core through SSH (uses normal SSH host-key verification):

```sh
python3 /path/to/herdr/scripts/core_terminal_bridge.py t1 --connect ssh -T box hc connect
```

Or pass a `machine/terminal` address from an already configured local core.
`--connect` consumes the remaining arguments as an argv, not a shell command.
The bridge inherits the transport's environment, including `HC_SESSION` and XDG
paths; keep those identical to the commands that created the test terminal.
Never point smoke tests at your normal core.

Input and pane size changes are forwarded. **Ctrl-] detaches**; it is reserved
by this PoC and cannot be sent literally to the program. Disconnecting or
closing the bridge does not kill the core terminal. Re-run the bridge manually
to reattach after a connection loss.

## Checks

```sh
just test-core-bridge
HERDR_CORE_SMOKE=1 just test-core-bridge

# Same PTY checks against an installed, unmodified remote core:
HERDR_CORE_SMOKE=1 HERDR_CORE_SMOKE_SSH=gx10 \
  HERDR_CORE_SMOKE_HC=/home/leon/.local/bin/hc just test-core-bridge
```

The opt-in live smoke needs an installed `hc`. It starts its own core with scratch
XDG paths under `/var/tmp`, checks output, input, resize, detach, terminal-mode
restoration and terminal survival, then stops only that core and removes its
scratch state. The SSH variant creates the scratch state on that remote machine,
uses a pipe (not an SSH PTY) for the framed transport, and cleans up that core and
its scratch state. Set `HERDR_CORE_SMOKE_HC` to the remote executable's path when
it is not on the noninteractive SSH PATH. Neither variant changes core code,
builds or installs the cloned core checkout, or replaces an existing server.

## Limits

- This is a transport proof, not a native `TerminalRuntime` backend or a new API
  method. Do not treat local process metadata or persisted pane state as the core
  terminal's identity. Herdr sees the bridge process, not the remote process.
- Workspace/tab persistence, terminal discovery UI, core events/status, explicit
  viewer focus, display facts, automatic reconnect, and Windows are not integrated.
- Automatic terminal replies use legacy DATA because the PTY does not preserve
  human-versus-automatic input provenance. They may claim viewer ownership.
- Use a disposable pane: cleanup restores Unix tty settings and basic screen
  modes, not every mode a full-screen application can change. Ctrl-] only handles
  the literal control byte, not enhanced keyboard encodings. Advanced keyboard,
  mouse, graphics and multi-viewer interactions are not validated.
- Nothing changes on existing Herdr remote connections. A later native adapter
  must address these gaps without altering the frozen Herdr endpoint codecs.
