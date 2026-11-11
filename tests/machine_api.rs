//! `--machine` routes a CLI command to that machine's local hc presentation
//! source. Presentation stays on this machine; only `hc connect` crosses SSH,
//! and only from the presenter. Most cases here stand in for the presenter so
//! they can observe exactly what the CLI sends; the last one runs the real one.
#![cfg(all(unix, not(target_os = "macos")))]

pub mod support;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const PROFILE_ID: &str = "0123456789abcdef0123456789abcdef";
const HOST_INSTANCE: &str = "presenter-instance-1";

fn app_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    }
}

struct Harness {
    root: PathBuf,
    state: PathBuf,
    presenter: UnixListener,
    local: UnixListener,
    protocol: u64,
}

impl Harness {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = PathBuf::from(format!(
            "/var/tmp/hma-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let state = root.join("state").join(app_dir_name()).join("client");
        let session = root
            .join("config")
            .join(app_dir_name())
            .join("sessions")
            .join(format!("hc-{PROFILE_ID}"));
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(&session).unwrap();
        fs::create_dir_all(root.join("bin")).unwrap();
        // The CLI must reach the machine through its presenter, never over SSH itself.
        fs::write(
            root.join("bin/ssh"),
            "#!/bin/sh\nprintf 'ssh\\n' >> \"$TEST_ROOT/ssh-calls\"\nexit 255\n",
        )
        .unwrap();
        fs::set_permissions(root.join("bin/ssh"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(
            state.join("endpoints.json"),
            serde_json::to_vec(&json!({
                "version": 1,
                "ssh": [
                    {"id": PROFILE_ID, "label": "mac", "target": "fake-mac", "session": "fleet", "enabled": true},
                    {"id": "fedcba9876543210fedcba9876543210", "label": "parked", "target": "fake-parked", "session": "fleet", "enabled": false}
                ]
            }))
            .unwrap(),
        )
        .unwrap();
        let presenter = UnixListener::bind(session.join("herdr.sock")).unwrap();
        presenter.set_nonblocking(true).unwrap();
        let local = UnixListener::bind(root.join("local.sock")).unwrap();
        local.set_nonblocking(true).unwrap();
        let status = Command::new(env!("CARGO_BIN_EXE_herdr"))
            .args(["status", "client", "--json"])
            .output()
            .unwrap();
        let status: Value = serde_json::from_slice(&status.stdout).unwrap();
        Self {
            root,
            state,
            presenter,
            local,
            protocol: status["protocol"].as_u64().unwrap(),
        }
    }

    /// A CLI invocation with a private Local hc registry on PATH, as a machine
    /// label must first be looked up there.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = self.command_without_hc(args);
        support::hc::configure(&mut command, &self.root.join("registry"));
        command
    }

    fn command_without_hc(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
        command
            .args(args)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.root.join("bin").display()),
            )
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_RUNTIME_DIR", &self.root)
            .env("TEST_ROOT", &self.root)
            .env("HERDR_SOCKET_PATH", self.root.join("local.sock"))
            .env(
                "HERDR_CLIENT_SOCKET_PATH",
                self.root.join("never-client.sock"),
            )
            .env("HERDR_SESSION", "wrong-inherited-session")
            .env("HERDR_PANE_ID", "wrong-local-pane")
            .env_remove("HERDR_CONFIG_PATH")
            .env_remove("HERDR_REMOTE_BINARY")
            .env_remove("HERDR_HC_SOURCE_PROFILE")
            .env_remove("HERDR_HC_SHARED_CORE")
            .env_remove("HERDR_HC_SHARED_REGISTRY_CORE")
            .env_remove("HC_SOCKET")
            .env_remove("HC_SESSION");
        command
    }

    /// Stands in for the presenter: answers the unscoped source preflight, then
    /// requires every request to be fenced to this host instance. Returns the
    /// first non-ping request, or the ping when `protocol` is 0. A null `result`
    /// drops that request's connection without replying.
    fn serve(&self, result: Value, protocol: u64) -> std::thread::JoinHandle<Value> {
        self.serve_source(
            json!({"result": {
                "type": "runtime_source", "profile_id": PROFILE_ID, "target": "fake-mac",
                "session": "fleet", "hc_path": null, "retired": false,
                "host_instance": HOST_INSTANCE
            }}),
            result,
            protocol,
        )
    }

    fn serve_source(
        &self,
        source: Value,
        result: Value,
        protocol: u64,
    ) -> std::thread::JoinHandle<Value> {
        let listener = self.presenter.try_clone().unwrap();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "presenter request did not arrive"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let outer: Value = serde_json::from_str(&line).unwrap();
                if outer["method"] == "runtime.source" {
                    let mut response = source.clone();
                    response["id"] = outer["id"].clone();
                    writeln!(stream, "{response}").unwrap();
                    if source.get("error").is_some() {
                        return outer;
                    }
                    continue;
                }
                assert_eq!(
                    outer["method"], "runtime.request",
                    "request was not fenced to the presenter: {outer}"
                );
                assert_eq!(outer["params"]["host_instance"], HOST_INSTANCE, "{outer}");
                let request = outer["params"]["request"].clone();
                let ping = request["method"] == "ping";
                if !ping && result.is_null() {
                    return request;
                }
                let response = if ping {
                    json!({"id": outer["id"], "result": {"type": "pong", "version": "test", "protocol": protocol}})
                } else {
                    let mut response = result.clone();
                    response["id"] = outer["id"].clone();
                    response
                };
                writeln!(stream, "{response}").unwrap();
                if !ping || protocol == 0 {
                    return request;
                }
            }
        })
    }

    /// Nothing reached Local, no client socket appeared, and the CLI itself
    /// never opened an SSH connection.
    fn assert_only_the_presenter_was_used(&self) {
        assert_eq!(
            self.local.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert!(!self.root.join("never-client.sock").exists());
        assert!(!self.root.join("ssh-calls").exists());
    }

    fn assert_no_pending_presenter_request(&self) {
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            self.presenter.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "an unexpected request reached the presenter"
        );
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = support::hc::cleanup(&self.root.join("registry"));
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn success(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn machine_api_routes_structured_payload_and_remote_errors_without_local_fallback() {
    let harness = Harness::new();
    let server = harness.serve(
        json!({"error":{"code":"test_remote_error","message":"remote rejected prompt"}}),
        harness.protocol,
    );
    let prompt = "quotes ' \" ; $(touch should-not-exist)\n--machine other";
    let output = harness
        .command(&["--machine", "mac", "agent", "prompt", "w4:p1", prompt])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let error: Value = serde_json::from_slice(&output.stderr)
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&output.stderr)));
    assert_eq!(error["error"]["code"], "test_remote_error");
    let request = server.join().unwrap();
    assert_eq!(request["method"], "agent.prompt");
    assert_eq!(request["params"]["text"], prompt);
    assert!(!harness.root.join("should-not-exist").exists());
    harness.assert_only_the_presenter_was_used();
}

#[test]
fn machine_api_profile_id_routes_large_list_responses_without_a_registry() {
    let harness = Harness::new();
    let data = "remote data ".repeat(20_000);
    let server = harness.serve(
        json!({"result":{"type":"agent_list","agents":[],"test_data":data}}),
        harness.protocol,
    );
    // An explicit profile ID keeps its meaning with no `hc` on PATH at all.
    let response = success(
        harness
            .command_without_hc(&["--machine", PROFILE_ID, "agent", "list"])
            .output()
            .unwrap(),
    );
    assert_eq!(response["result"]["test_data"], data);
    assert_eq!(server.join().unwrap()["method"], "agent.list");
    harness.assert_only_the_presenter_was_used();
}

#[test]
fn machine_api_label_fails_closed_when_the_shared_registry_is_unavailable() {
    let harness = Harness::new();
    let output = harness
        .command_without_hc(&["--machine", "mac", "agent", "list"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("shared hc registry unavailable; no legacy source was selected"),
        "{error}"
    );
    harness.assert_no_pending_presenter_request();
    harness.assert_only_the_presenter_was_used();
}

#[test]
fn machine_api_never_replays_a_mutation_when_its_response_is_lost() {
    let harness = Harness::new();
    let server = harness.serve(Value::Null, harness.protocol);
    let output = harness
        .command(&["--machine", "mac", "pane", "close", "w4:p1"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(server.join().unwrap()["method"], "pane.close");
    harness.assert_no_pending_presenter_request();
    harness.assert_only_the_presenter_was_used();
}

#[test]
fn machine_api_refuses_a_local_session_that_is_not_its_presentation_source() {
    let harness = Harness::new();
    // Some other server owns the `hc-<profile>` session name.
    let server = harness.serve_source(
        json!({"error":{"code":"unsupported_runtime","message":"hc source is unavailable"}}),
        json!({"result":{"type":"ok"}}),
        harness.protocol,
    );
    let output = harness
        .command(&["--machine", "mac", "pane", "close", "w4:p1"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("machine 'mac'"), "{error}");
    assert!(
        error.contains("is not an hc presentation source; it was not replaced"),
        "{error}"
    );
    assert_eq!(server.join().unwrap()["method"], "runtime.source");
    harness.assert_no_pending_presenter_request();
    harness.assert_only_the_presenter_was_used();
}

#[test]
fn machine_remove_deletes_only_that_profiles_metadata() {
    let harness = Harness::new();
    let directory = harness.state.join("ssh-metadata");
    fs::create_dir_all(&directory).unwrap();
    let own = directory.join(format!("{PROFILE_ID}.json"));
    let other = directory.join("fedcba9876543210fedcba9876543210.json");
    fs::write(&own, "this machine metadata").unwrap();
    fs::write(&other, "other machine metadata").unwrap();
    let output = harness
        .command(&["machine", "remove", PROFILE_ID])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!own.exists());
    assert_eq!(fs::read_to_string(other).unwrap(), "other machine metadata");
    harness.assert_no_pending_presenter_request();
    harness.assert_only_the_presenter_was_used();
}

#[test]
fn machine_api_never_inherits_the_callers_pane() {
    let harness = Harness::new();
    let server = harness.serve(json!({"result":{"type":"ok"}}), harness.protocol);
    success(
        harness
            .command(&["--machine=mac", "pane", "current"])
            .output()
            .unwrap(),
    );
    let request = server.join().unwrap();
    assert_eq!(request["method"], "pane.current");
    assert!(request["params"]["caller_pane_id"].is_null());
    harness.assert_only_the_presenter_was_used();
}

#[test]
fn machine_api_remote_paths_and_wait_parameters_reach_the_server() {
    for (args, method, expected) in [
        (
            vec![
                "worktree",
                "create",
                "--cwd",
                "~/Projects/herdr",
                "--branch",
                "review",
                "--path",
                "/srv/review",
            ],
            "worktree.create",
            json!({"cwd":"~/Projects/herdr", "branch":"review", "path":"/srv/review"}),
        ),
        (
            vec![
                "agent",
                "wait",
                "w4:p1",
                "--until",
                "idle",
                "--timeout",
                "2000",
            ],
            "agent.wait",
            json!({"target":"w4:p1", "until":["idle"], "timeout_ms":2000}),
        ),
    ] {
        let harness = Harness::new();
        let server = harness.serve(json!({"result":{"type":"ok"}}), harness.protocol);
        let mut command = harness.command(&["--machine", "mac"]);
        success(command.args(args).output().unwrap());
        let request = server.join().unwrap();
        assert_eq!(request["method"], method);
        for (key, value) in expected.as_object().unwrap() {
            assert_eq!(&request["params"][key], value);
        }
        harness.assert_only_the_presenter_was_used();
    }
}

#[test]
fn machine_api_status_reports_remote_identity_not_local_installation_state() {
    let harness = Harness::new();
    let server = harness.serve(json!({}), 0);
    let status = success(
        harness
            .command(&["--machine", "mac", "status", "server", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(status["session"], "fleet");
    assert_eq!(status["socket"], format!("machine:{PROFILE_ID}/fleet"));
    assert!(status["server_binary_stale"].is_null());
    assert_eq!(server.join().unwrap()["method"], "ping");
    harness.assert_only_the_presenter_was_used();
}

#[test]
fn machine_api_usage_errors_do_not_connect() {
    let harness = Harness::new();
    for args in [
        vec!["--machine", "missing", "agent", "list"],
        vec!["--machine", "parked", "agent", "list"],
        vec![
            "--machine",
            "fedcba9876543210fedcba9876543210",
            "agent",
            "list",
        ],
        vec!["--machine", "mac", "config", "reset-keys"],
        vec![
            "--machine",
            "mac",
            "agent",
            "explain",
            "--file",
            "/any/file",
            "--agent",
            "pi",
        ],
        vec![
            "--machine",
            "mac",
            "pane",
            "split",
            "--current",
            "--direction",
            "right",
        ],
        vec!["--machine", "mac", "--session", "local", "agent", "list"],
    ] {
        let output = harness.command(&args).output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    harness.assert_no_pending_presenter_request();
    harness.assert_only_the_presenter_was_used();
}

#[test]
fn machine_api_server_stop_is_sent_only_to_the_selected_machine() {
    let harness = Harness::new();
    let server = harness.serve(json!({"result":{"type":"ok"}}), harness.protocol);
    let output = harness
        .command(&["--machine", "mac", "server", "stop"])
        .env("HERDR_SOCKET_PATH", harness.root.join("missing-local.sock"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(server.join().unwrap()["method"], "server.stop");
    harness.assert_only_the_presenter_was_used();
}

#[test]
fn machine_api_protocol_mismatch_never_sends_the_mutation() {
    let harness = Harness::new();
    let server = harness.serve(json!({}), 0);
    let output = harness
        .command(&["--machine", "mac", "pane", "close", "w4:p1"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(server.join().unwrap()["method"], "ping");
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("protocol_mismatch"), "{error}");
    // The guidance restarts only this machine's local presenter.
    assert!(
        error.contains(&format!("herdr --machine {PROFILE_ID} server stop")),
        "{error}"
    );
    assert!(!error.contains("HERDR_SOCKET_PATH="), "{error}");
    harness.assert_no_pending_presenter_request();
    harness.assert_only_the_presenter_was_used();
}

fn presenter_socket(root: &Path) -> PathBuf {
    root.join("config")
        .join(app_dir_name())
        .join("sessions")
        .join(format!("hc-{PROFILE_ID}"))
        .join("herdr.sock")
}

fn presenter_is_reachable(root: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(presenter_socket(root)).is_ok()
}

/// The real path end to end: the CLI starts the machine's local presenter, the
/// presenter reaches a second private core through an hc-only SSH shim, and
/// stopping the presenter only detaches.
#[test]
fn machine_api_starts_a_local_presenter_and_only_hc_connect_crosses_ssh() {
    use std::os::unix::fs::DirBuilderExt;

    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    // The presenter's client socket path includes the 32-character profile ID.
    let root = PathBuf::from(format!("/var/tmp/hme-{:08x}", nonce as u32));
    fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
    let source_runtime = root.join("source");
    let state = root.join("state").join(app_dir_name()).join("client");
    fs::create_dir_all(&state).unwrap();
    fs::create_dir_all(root.join("config").join(app_dir_name())).unwrap();
    fs::write(
        root.join("config").join(app_dir_name()).join("config.toml"),
        "onboarding = false\n",
    )
    .unwrap();
    fs::write(
        state.join("endpoints.json"),
        json!({
            "version": 1,
            "ssh": [{"id": PROFILE_ID, "label": "mac", "target": "test-only", "session": "fixture", "enabled": true}]
        })
        .to_string(),
    )
    .unwrap();

    let bin = root.join("bin");
    let source_bin = root.join("home/.local/bin");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&source_bin).unwrap();
    std::os::unix::fs::symlink(
        support::hc::private_cli(&source_runtime),
        source_bin.join("hc"),
    )
    .unwrap();
    let quote = |path: &Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
    let ssh_commands = root.join("ssh-commands");
    let ssh_args = root.join("ssh-args");
    fs::write(
        bin.join("ssh"),
        format!(
            "#!/bin/sh\nexport HOME={}\ncontrol=; previous=; configuration=\nfor arg do\n  printf '%s\\n' \"$arg\" >> {}\n  if [ \"$previous\" = -S ]; then control=$arg; fi\n  if [ \"$arg\" = -G ]; then configuration=yes; fi\n  previous=$arg; last=$arg\ndone\nif [ -n \"$configuration\" ]; then printf 'controlpath %s\\n' \"$control\"; exit 0; fi\ncase \"$last\" in *'exec hc connect') ;; *) echo 'unexpected non-hc SSH command' >&2; exit 93;; esac\nprintf '%s\\n' \"$last\" >> {}\nexec /bin/sh -c \"$last\"\n",
            quote(&root.join("home")),
            quote(&ssh_args),
            quote(&ssh_commands),
        ),
    )
    .unwrap();
    fs::set_permissions(bin.join("ssh"), fs::Permissions::from_mode(0o700)).unwrap();

    support::register_runtime_dir(&root);
    let local_socket = root.join("local.sock");
    let cli = |args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
        command
            .args(["--machine", PROFILE_ID])
            .args(args)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("HOME", root.join("home"))
            .env("SHELL", "/bin/sh")
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_RUNTIME_DIR", &root)
            .env("HERDR_SOCKET_PATH", &local_socket);
        for key in [
            "HERDR_CLIENT_SOCKET_PATH",
            "HERDR_CONFIG_PATH",
            "HERDR_ENV",
            "HERDR_SESSION",
            "HERDR_PANE_ID",
            "HERDR_HC_SOURCE_PROFILE",
            "HERDR_HC_SHARED_CORE",
            "HERDR_HC_SHARED_REGISTRY_CORE",
            "HC_SOCKET",
            "HC_SESSION",
            "HC_TERMINAL_ID",
            "HC_TERMINAL_SOCKET",
        ] {
            command.env_remove(key);
        }
        command.output().unwrap()
    };

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let created = success(cli(&["workspace", "create", "--cwd", "/var/tmp"]));
        assert_eq!(created["result"]["type"], "workspace_created");
        assert!(presenter_is_reachable(&root));
        assert!(
            !local_socket.exists(),
            "routing to a machine must not start Local"
        );

        // The program runs in the machine's own core, not in a local one.
        let mut programs = Vec::new();
        assert!(
            support::wait_until(Duration::from_secs(10), Duration::from_millis(50), || {
                programs = support::hc::terminals(&source_runtime);
                programs.len() == 1 && programs[0]["state"] == "live"
            }),
            "source program was not started: {programs:?}"
        );
        let source_pid = programs[0]["pid"].clone();
        assert!(source_pid.as_u64().is_some());

        let listed = success(cli(&["workspace", "list"]));
        assert_eq!(
            listed["result"]["workspaces"].as_array().map(Vec::len),
            Some(1)
        );

        let commands = fs::read_to_string(&ssh_commands).unwrap();
        assert!(
            commands
                .lines()
                .all(|line| line.ends_with("exec hc connect")),
            "{commands}"
        );
        assert!(!commands.contains("herdr"), "{commands}");
        let args = fs::read_to_string(&ssh_args).unwrap();
        for policy in ["StrictHostKeyChecking=yes", "BatchMode=yes"] {
            assert!(args.lines().any(|arg| arg == policy), "{policy}: {args}");
        }

        // Stopping the presenter detaches; the machine's program keeps running.
        let stopped = cli(&["server", "stop"]);
        assert!(
            stopped.status.success(),
            "{}",
            String::from_utf8_lossy(&stopped.stderr)
        );
        assert!(
            support::wait_until(Duration::from_secs(10), Duration::from_millis(50), || {
                !presenter_is_reachable(&root)
            }),
            "presenter did not stop"
        );
        let after = support::hc::terminals(&source_runtime);
        assert_eq!(after.len(), 1, "{after:?}");
        assert_eq!(after[0]["state"], "live");
        assert_eq!(after[0]["pid"], source_pid);
        assert!(!local_socket.exists());
    }));

    if presenter_is_reachable(&root) {
        let _ = cli(&["server", "stop"]);
        support::wait_until(Duration::from_secs(10), Duration::from_millis(50), || {
            !presenter_is_reachable(&root)
        });
    }
    let cleaned = support::hc::cleanup(&source_runtime);
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
    cleaned.expect("source core cleanup");
    support::unregister_runtime_dir(&root);
    fs::remove_dir_all(&root).unwrap();
}
