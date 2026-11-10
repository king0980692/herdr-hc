# Subprocess runtime tests

Unix subprocess tests need upstream `hc` on the parent test runner's `PATH`.
CI builds hc 0.1.18 from commit `9ce385d9c8abb041dd4437476d770365b3fb5282`
(including its pinned Ghostty submodule); there is no fake backend or silent skip
when hc is missing. Use Zig 0.16.0 for builds. Building the pinned hc from source
on macOS also requires full Xcode: its Ghostty build emits an XCFramework.
Command Line Tools alone cannot run that step. Xcode's first-launch components
must also be installed; `xcodebuild -version` alone does not prove XCFramework
packaging works. If full Xcode is installed but not selected, set
`DEVELOPER_DIR` for the build job rather than changing the machine-wide
developer directory.

Before spawning a runtime-capable Herdr process, call
`support::hc::configure_pty` or `support::hc::configure` after setting any custom
PATH/HOME. Each registered Herdr runtime directory gets a private hc wrapper,
config, state, HOME and short Unix socket root. Presenter restarts reuse that
core. Explicit test HOME/PATH overrides are preserved; inherited hc routing and
saved-source activation are removed. Do not invoke an unconfigured hc process.

Stop/drop owned presenters before `cleanup_test_base`: macOS does not use Linux
`/proc` discovery. That helper cleans the standard `base/runtime` private core.
Explicitly clean and unregister additional runtime keys before removing the base
(e.g. federation's separate source). Cleanup errors fail the test and retain
scratch evidence. Panic/exit hooks also attempt bounded cleanup of registered
resources, resuming stopped owned processes before termination; they never stop
a normal core. A failed atexit cleanup exits nonzero instead of reporting a green
test process. Static Rust globals do not run Drop at exit; the installed atexit
callback, not a static guard, owns that path.

Cleanup has one 15-second budget per private root. It requires explicit JSON
`stopped: true` and bounded probes of that root's core/holder sockets before
removing evidence. `no core running` is not proof that its holder is gone: allow
the holder's own recovery to finish and stop that existing core, never start a
core or launch a program for cleanup. Upstream stop ends even kept programs;
their saved pending records are not permission to respawn them. Failure, timeout
or uncertain endpoint ownership retains the root and registry entry for retry.
Only a timed-out stopper CLI is killed; there is no hc PID-scan/kill fallback.

`hc_fixture` covers independent/reused cores, unknown-runtime no-op, every private
source session (including 64-byte holder paths), kept-program/core-crash cleanup
without replacement launches, failed/empty-success/timed-out stops and retry,
process exit/panic/SIGINT/SIGTERM, failed atexit exit status, continued cleanup of
other registered roots, and preservation of an independent core throughout.
Both command builders preserve explicit HOME/PATH without changing process-wide
routing. None of these tests involves normal local or remote sessions.

Audited Unix launch entry points are the server/viewer helpers in `api_ping`,
`auto_detect`, `client_mode`, `cross_area`, `detach_reattach`, `live_handoff`,
`multi_client`, `server_headless`, `session_restore`, the opt-in `host_shutdown`
fixture, and `cli/harness`. Runtime-capable launches use the private configure
helpers; direct command-only cases target existing/fake sockets, exercise early
validation, or stop before runtime creation. `machine_api`, `machine_setup`,
`remote_attach`, `session_delete` and CLI contract captures do not launch a real
remote/source runtime. Windows fixture qualification is separate and not covered
by this Unix audit.

Do not deliberately daemonize/detach unregistered descendants from fixture
programs: hc stop is not a general descendant sweep. SIGKILL, abort and machine
loss cannot execute in-process cleanup hooks; retained scratch evidence is not
a promise of automatic recovery from those cases.

The opt-in native smoke catches assertion unwinds around both runtime and saved
presentation exercises. It confirms the local presenter exits (polling only the
PID obtained from its private socket peer) before stopping/removing its source
core. Unconfirmed presenter cleanup retains both roots. Smoke CLI timeouts kill
only their own helper processes. The Python bridge PoC likewise retains scratch
on failed/unconfirmed stop instead of a TemporaryDirectory deleting it anyway.

Native source cleanup uses one 15-second deadline and probes nested runtime
socket directories without following directory symlinks before deleting local
scratch. Fault tests retain evidence for a reachable nested socket and reject
cleanup requests to an unrecorded or replacement presentation owner. A presenter
that retires and exits itself is accepted only with its previously recorded
private socket-peer PID and an absent/refused endpoint.

The saved-source live smoke retargets, disables and removes only its private
catalog entry. Each case checks bounded presenter exit, persisted exact binding,
unchanged hc address/PID, public-ID preservation on attach-only restart, and
rejection of launch/input through the old host instance. The core replacement
case gates only private read-only SSH hello attempts, explicitly stops core A,
and archives only its stopped private state after its socket files disappear.
A fresh pinned core B starts with no programs; the fixture explicitly occupies
A's old address to verify that the different permanent core ID prevents input,
close, adoption or relaunch. Workspace/tab/pane public IDs and A's raw binding
survive restart, while B's programs survive presenter retirement and detach.
This is local-source qualification through an hc-only SSH shim, not evidence of
real-remote, Windows or fresh Linux qualification. The real-PTY federation tests
separately exercise two independent private cores, source loss/recovery, stalled
source isolation, selection and mouse/input preservation.

Source status fixtures emit OSC 7501 from the actual program. Legacy
`pane.report_agent`/metadata hooks must stay rejected for hc-owned panes; do not
restore local PID or screen detection to make old assertions pass.

Use `support::drain_pty` when asserting graceful exit of a PTY-backed server:
unread diagnostics can otherwise hold macOS in terminal teardown. Do not use
that helper in tests whose purpose is closing the master to produce a hangup.

Machine-setup tests may fake the read-only SSH hello; they assert that no
terminal or remote Herdr operation follows it. Program/runtime tests still use
the real isolated hc backend above.

`live_handoff` now also houses explicit hc presentation-restart contracts.
`restart_presentation` stops and starts the presenter, checking the permanent
core ID and complete source address/PID set before/after both operations. It is
not a successful PTY transfer. Keep the actual child I/O/no-HUP/no-replay,
keyboard-byte, plugin, named-socket and HTTP assertions. PTY-transfer rejection
is a separate contract.

The federation fixture owns its local `hc-<profile>` presenter and carries only
`hc connect` through the SSH shim to a second private core. It must not start
remote Herdr or an unwanted Local server. Transport/presenter suspension tests
must register suspended processes for panic-hook cleanup, not rely only on Drop
(the panic hook runs before unwinding).

`agent.start` must wait for source foreground evidence of an available shell,
not inspect a source PID on the client. A submitted launch intent can be listed
as pending without fabricating an agent app/status/readiness. Tests may retry a
pre-submission `agent_pane_busy` rejection, never a timeout or unknown outcome.
