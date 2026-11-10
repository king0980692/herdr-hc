#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

// No network/runtime: saved setup ends at hc hello; explicit legacy attach fails at startup.
const SSH: &str = r#"#!/bin/sh
for arg do
    last=$arg
    if [ "$arg" = 'StrictHostKeyChecking=yes' ]; then strict_host_key_check=yes; fi
done
case "$last" in
    *' connect')
        echo hc-connect >>"$FAKE_ROOT/command-order"
        printf '%s\n' "$@" >"$FAKE_ROOT/ssh-args"
        echo hc-connect >>"$FAKE_ROOT/probes"
        printf '%s\n' "$last" >"$FAKE_ROOT/start-command"
        if [ "$FAKE_WINDOWS_TRANSPORT_FAILURE" = yes ]; then echo 'ssh: connect to host fake-host port 22: Connection refused' >&2; exit 255; fi
        if [ "$FAKE_STRICT_HOST_KEY_FAILURE" = yes ] && [ "$strict_host_key_check" = yes ]; then
            echo 'Host key verification failed.' >&2; exit 255
        fi
        if [ "$FAKE_INSTALLED" = missing ]; then echo 'hc: not found' >&2; exit 127; fi
        IFS= read -r hello
        printf '%s\n' "$hello" >>"$FAKE_ROOT/source-requests"
        printf '%s\n' "$FAKE_HC_HELLO"
        # The setup check must disconnect after hello, not launch a terminal.
        while IFS= read -r request; do printf '%s\n' "$request" >>"$FAKE_ROOT/source-requests"; done
        exit 0 ;;
esac
case "$*" in
    *powershell.exe*) echo windows >>"$FAKE_ROOT/command-order" ;;
    *'/bin/sh -s'*) echo posix-probe >>"$FAKE_ROOT/command-order" ;;
esac
if [ "$FAKE_STRICT_HOST_KEY_FAILURE" = yes ] && [ "$strict_host_key_check" = yes ]; then
    echo 'No RSA host key is known for fake-host and you have requested strict checking.' >&2
    echo 'Host key verification failed.' >&2
    exit 255
fi
case "$*" in
    *powershell.exe*)
        if [ "$FAKE_WINDOWS_TRANSPORT_FAILURE" = yes ]; then echo 'ssh: connect to host fake-host port 22: Connection refused' >&2; exit 255; fi
        if [ "$FAKE_WINDOWS" = yes ]; then printf '\n%s\n%s\n' 'herdr-remote-output-ready:1' 'herdr-windows:AMD64'; exit 0; fi
        exit 1 ;;
esac
if [ "$last" = 'command -v herdr' ]; then
    if [ "$FAKE_INSTALLED" != missing ]; then echo /home/remote/.local/bin/herdr; fi
    exit 0
fi
case "$last" in
    'tee '*) cat >/dev/null; exit 0 ;;
esac
case "$last" in
    *'herdr-remote-output-ready:1'*) script=$last ;;
    *) script=$(cat) ;;
esac
printf '\n%s\n' 'herdr-remote-output-ready:1'
case "$script" in
    *'uname -s'*) echo platform >>"$FAKE_ROOT/probes"; uname -s; uname -m ;;
    *'version='*)
        echo candidates >>"$FAKE_ROOT/probes"
        if [ "$FAKE_INSTALLED" != missing ]; then echo /home/remote/.local/bin/herdr; fi ;;
    *'status client --json'*)
        if [ "$FAKE_INSTALLED" = new ] || [ -f "$FAKE_ROOT/installed" ]; then
            printf '%s\n' "$FAKE_CLIENT_STATUS"
        else
            echo '{"version":"0.8.2","protocol":20}'
        fi ;;
    *'status server --json'*)
        if [ -f "$FAKE_ROOT/stopped" ]; then
            echo '{"running":false}'
        elif [ "$FAKE_STRICT_HOST_KEY_FAILURE" = yes ]; then
            echo '{"running":true,"version":"0.8.2","capabilities":{"live_handoff":true,"detached_server_daemon":true,"endpoint_protocol_generation":1,"surface_interest":true,"health_check":true}}'
        else
            echo '{"running":true,"version":"0.8.2","capabilities":{"live_handoff":true,"detached_server_daemon":true}}'
        fi ;;
    *'server live-handoff'*) echo handoff >>"$FAKE_ROOT/actions"; exit 1 ;;
    *'server stop'*) echo stop >>"$FAKE_ROOT/actions"; touch "$FAKE_ROOT/stopped" ;;
    *'session list --json'*)
        echo sessions >>"$FAKE_ROOT/probes"
        if [ "$FAKE_SESSIONS" = failed ]; then echo 'session query denied' >&2; exit 1; fi
        printf '%s\n' "$FAKE_SESSIONS" ;;
    *'remote-client-bridge'*)
        printf '%s\n' "$script" >"$FAKE_ROOT/start-command"
        echo start >>"$FAKE_ROOT/actions"; echo 'test startup failure' >&2; exit 1 ;;
    *'mkdir -p'*) printf '/fake/tmp\000/fake/herdr\000' ;;
    *'chmod 755'*) echo install >>"$FAKE_ROOT/actions"; touch "$FAKE_ROOT/installed" ;;
    *'command -v herdr'*) if [ "$FAKE_INSTALLED" != missing ]; then echo /home/remote/.local/bin/herdr; fi ;;
    *'test -x '*) exit 1 ;;
    *) echo "unexpected fake SSH script: $script" >&2; exit 1 ;;
esac
"#;

struct SetupResult {
    output: String,
    actions: String,
    prompts: usize,
    success: bool,
    probes: String,
    command_order: String,
    start_command: String,
    catalog: Option<serde_json::Value>,
    source_requests: Vec<serde_json::Value>,
    ssh_args: String,
}

#[derive(Default)]
struct SetupOptions<'a> {
    sessions: Option<&'a str>,
    selection: &'a [u8],
    explicit_session: Option<&'a str>,
    hc_path: Option<&'a str>,
    noninteractive: bool,
    windows: bool,
    windows_transport_failure: bool,
    hello: Option<&'a str>,
}

fn setup(installed: &str, answer: &str, handoff: bool) -> SetupResult {
    setup_with_strict_host_key_failure(installed, answer, handoff, false)
}

fn setup_with_strict_host_key_failure(
    installed: &str,
    answer: &str,
    handoff: bool,
    strict_host_key_failure: bool,
) -> SetupResult {
    setup_options(
        installed,
        answer,
        handoff,
        strict_host_key_failure,
        SetupOptions::default(),
    )
}

fn setup_options(
    installed: &str,
    answer: &str,
    handoff: bool,
    strict_host_key_failure: bool,
    options: SetupOptions<'_>,
) -> SetupResult {
    let root = std::path::PathBuf::from(format!(
        "/var/tmp/herdr-machine-setup-{}-{}-{}-{}-{}-{}",
        std::process::id(),
        installed,
        answer.trim().is_empty(),
        handoff,
        strict_host_key_failure,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let app = if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    };
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::create_dir_all(root.join("config").join(app)).unwrap();
    fs::write(root.join("bin/ssh"), SSH).unwrap();
    fs::set_permissions(root.join("bin/ssh"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(
        root.join("config").join(app).join("config.toml"),
        "onboarding = false\n[remote]\nmanage_ssh_config = false\n",
    )
    .unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_herdr"))
        .args(["status", "client", "--json"])
        .output()
        .unwrap();
    assert!(status.status.success());

    let pair = native_pty_system().openpty(PtySize::default()).unwrap();
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_herdr"));
    if handoff {
        command.args(["--remote", "fake-host", "--handoff"]);
    } else {
        command.args(["machine", "add", "fake-host", "--label", "VPS"]);
        if let Some(session) = options.explicit_session {
            command.args(["--remote-session", session]);
        }
        if let Some(path) = options.hc_path {
            command.args(["--hc-path", path]);
        }
    }
    command.env(
        "PATH",
        format!("{}:/usr/bin:/bin", root.join("bin").display()),
    );
    command.env("HOME", &root);
    command.env("XDG_CONFIG_HOME", root.join("config"));
    command.env("XDG_STATE_HOME", root.join("state"));
    command.env("XDG_RUNTIME_DIR", &root);
    command.env("FAKE_ROOT", &root);
    command.env("FAKE_INSTALLED", installed);
    command.env("FAKE_HC_HELLO",options.hello.unwrap_or(r#"{"jsonrpc":"2.0","id":1,"result":{"generation":1,"framed":true,"core":{"id":"setup-fixture","name":"fake-host"},"version":"0.1.18","methods":[]}}"#));
    command.env(
        "FAKE_SESSIONS",
        options
            .sessions
            .unwrap_or(r#"{"sessions":[{"name":"default","running":true}]}"#),
    );
    command.env(
        "FAKE_STRICT_HOST_KEY_FAILURE",
        if strict_host_key_failure { "yes" } else { "no" },
    );
    command.env("FAKE_WINDOWS", if options.windows { "yes" } else { "no" });
    command.env(
        "FAKE_WINDOWS_TRANSPORT_FAILURE",
        if options.windows_transport_failure {
            "yes"
        } else {
            "no"
        },
    );
    command.env(
        "FAKE_CLIENT_STATUS",
        String::from_utf8(status.stdout).unwrap(),
    );
    for name in [
        "HERDR_ENV",
        "HERDR_SESSION",
        "HERDR_SOCKET_PATH",
        "HERDR_CLIENT_SOCKET_PATH",
        "HERDR_REMOTE_BINARY",
        "HERDR_CONFIG_PATH",
        "HERDR_HC_SOURCE_PROFILE",
        "HC_SOCKET",
        "HC_TERMINAL_ID",
        "HC_SESSION",
    ] {
        command.env_remove(name);
    }
    if options.noninteractive {
        // A redirected stdin makes machine add non-interactive while retaining
        // the same isolated SSH fixture and captured terminal diagnostics.
        let mut shell = CommandBuilder::new("/bin/sh");
        shell.args(["-c", "exec \"$@\" </dev/null", "sh"]);
        shell.args(command.get_argv());
        shell.env_clear();
        for (key, value) in command.iter_full_env_as_str() {
            shell.env(key, value);
        }
        command = shell;
    }
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
    let (tx, rx) = mpsc::channel();
    let reading = std::thread::spawn(move || {
        let mut buffer = [0; 4096];
        while let Ok(len) = reader.read(&mut buffer) {
            if len == 0 || tx.send(buffer[..len].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut output = String::new();
    let mut prompts = 0;
    let mut timed_out = false;
    let mut picked = false;
    loop {
        match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(bytes) => {
                output.push_str(&String::from_utf8_lossy(&bytes));
                if !picked && output.contains("Esc cancel") {
                    writer.write_all(options.selection).unwrap();
                    writer.flush().unwrap();
                    picked = true;
                }
                let count = output.matches("[y/N]").count() + output.matches("[Y/n]").count();
                if count > prompts {
                    writer
                        .write_all(if prompts == 0 {
                            answer.as_bytes()
                        } else {
                            b"n\n"
                        })
                        .unwrap();
                    writer.flush().unwrap();
                    prompts = count;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                timed_out = true;
                child.kill().unwrap();
                break;
            }
        }
    }
    let status = child.wait().unwrap();
    drop(writer);
    drop(pair.master);
    reading.join().unwrap();
    let actions = fs::read_to_string(root.join("actions")).unwrap_or_default();
    let probes = fs::read_to_string(root.join("probes")).unwrap_or_default();
    let command_order = fs::read_to_string(root.join("command-order")).unwrap_or_default();
    let start_command = fs::read_to_string(root.join("start-command")).unwrap_or_default();
    let catalog = fs::read(root.join("state").join(app).join("client/endpoints.json"))
        .ok()
        .map(|bytes| serde_json::from_slice(&bytes).unwrap());
    let ssh_args = fs::read_to_string(root.join("ssh-args")).unwrap_or_default();
    let source_requests = fs::read_to_string(root.join("source-requests"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).expect("source must send JSON hello only"))
        .collect();
    fs::remove_dir_all(root).unwrap();
    assert_eq!(
        catalog.is_some(),
        status.success() && !handoff,
        "setup saved an unexpected catalog: {output}"
    );
    assert!(!timed_out, "setup timed out: {output}");
    SetupResult {
        output,
        actions,
        prompts,
        success: status.success(),
        probes,
        command_order,
        start_command,
        catalog,
        source_requests,
        ssh_args,
    }
}

#[test]
fn machine_add_accepts_help_argument_order() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "herdr-machine-add-order-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&root).unwrap();
    let app = if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    };
    fs::create_dir_all(root.join("config").join(app)).unwrap();
    fs::write(
        root.join("config").join(app).join("config.toml"),
        "onboarding = false\n[remote]\nmanage_ssh_config = false\n",
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
    command.args(["machine", "add", "--label", "coder", "workstation.coder"]);
    // Reach remote preparation, but never execute SSH or start a server.
    command.env("PATH", root.join("no-executables"));
    command.env("HOME", &root);
    command.env("XDG_CONFIG_HOME", root.join("config"));
    command.env("XDG_STATE_HOME", root.join("state"));
    command.env("XDG_RUNTIME_DIR", &root);
    for name in [
        "HERDR_ENV",
        "HERDR_SESSION",
        "HERDR_SOCKET_PATH",
        "HERDR_CLIENT_SOCKET_PATH",
        "HERDR_REMOTE_BINARY",
        "HERDR_CONFIG_PATH",
    ] {
        command.env_remove(name);
    }
    let output = command.output().unwrap();
    let saved = root
        .join("state")
        .join(app)
        .join("client/endpoints.json")
        .exists();
    fs::remove_dir_all(root).unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("machine was not saved"), "{stderr}");
    assert!(!saved, "failed preparation must not save a machine");
}

fn assert_hc_only(result: &SetupResult, session: &str) {
    assert_hc_only_at(result, session, None);
}

fn assert_hc_only_at(result: &SetupResult, session: &str, hc_path: Option<&str>) {
    assert!(result.success, "{}", result.output);
    assert_eq!(result.prompts, 0, "{}", result.output);
    assert_eq!(result.probes, "hc-connect\n", "{}", result.output);
    assert_eq!(result.command_order, "hc-connect\n", "{}", result.output);
    assert!(result.actions.is_empty(), "{}", result.actions);
    assert!(
        !result.output.contains("Running sessions"),
        "{}",
        result.output
    );
    let profiles = result.catalog.as_ref().unwrap()["ssh"].as_array().unwrap();
    assert_eq!(profiles.len(), 1);
    assert_eq!(profiles[0]["target"], "fake-host");
    assert_eq!(profiles[0]["session"], session);
    assert_eq!(profiles[0]["label"], "VPS");
    assert_eq!(profiles[0]["enabled"], true);
    let executable = hc_path.map_or_else(
        || "hc".to_owned(),
        |path| {
            if path
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || "/._-".contains(ch))
            {
                path.to_owned()
            } else {
                format!("'{}'", path.replace('\'', "'\\''"))
            }
        },
    );
    assert!(
        result
            .start_command
            .trim_end()
            .ends_with(&format!("exec {executable} connect")),
        "{}",
        result.start_command
    );
    assert_eq!(
        profiles[0].get("hc_path").and_then(|value| value.as_str()),
        hc_path
    );
    assert!(!result.start_command.contains("remote-client-bridge"));
    assert_eq!(
        result.source_requests.len(),
        1,
        "{:?}",
        result.source_requests
    );
    assert_eq!(result.source_requests[0]["method"], "hello");
    assert_eq!(result.source_requests[0]["params"]["generation"], 1);
    assert_eq!(result.source_requests[0]["params"]["framed"], true);
    for option in [
        "StrictHostKeyChecking=yes",
        "BatchMode=yes",
        "NumberOfPasswordPrompts=0",
    ] {
        assert!(
            result.ssh_args.lines().any(|arg| arg == option),
            "{}",
            result.ssh_args
        );
    }
}

#[test]
fn machine_add_custom_hc_path_only_sends_hello_and_persists_the_path() {
    for path in ["/opt/hc-pinned", "/opt/hc with 'quotes';not-a-command"] {
        let result = setup_options(
            "new",
            "",
            false,
            false,
            SetupOptions {
                hc_path: Some(path),
                explicit_session: Some("agents"),
                ..Default::default()
            },
        );
        assert_hc_only_at(&result, "agents", Some(path));
    }
}

#[test]
fn machine_add_missing_custom_hc_never_falls_back_or_saves() {
    let result = setup_options(
        "missing",
        "",
        false,
        false,
        SetupOptions {
            hc_path: Some("/missing/hc"),
            noninteractive: true,
            ..Default::default()
        },
    );
    assert!(!result.success, "{}", result.output);
    assert!(result.catalog.is_none());
    assert_eq!(result.probes, "hc-connect\n");
    assert!(result
        .start_command
        .trim_end()
        .ends_with("exec /missing/hc connect"));
    assert!(!result.start_command.contains("exec hc connect"));
    assert!(result.source_requests.is_empty());
    assert!(result.actions.is_empty());
}

#[test]
fn machine_add_invalid_hc_path_does_not_contact_ssh() {
    let result = setup_options(
        "new",
        "",
        false,
        false,
        SetupOptions {
            hc_path: Some("relative/hc"),
            ..Default::default()
        },
    );
    assert!(!result.success, "{}", result.output);
    assert!(result.catalog.is_none());
    assert!(result.probes.is_empty());
    assert!(result.source_requests.is_empty());
    assert!(result.actions.is_empty());
}

#[test]
fn machine_add_missing_hc_fails_without_installing_or_saving() {
    let result = setup("missing", "y\n", false);
    assert!(!result.success, "{}", result.output);
    assert!(result.catalog.is_none());
    assert_eq!(result.prompts, 0);
    assert_eq!(result.probes, "hc-connect\n");
    assert!(result.actions.is_empty());
    assert!(result.source_requests.is_empty());
    assert!(result.output.contains("hc: not found"), "{}", result.output);
    assert!(result.output.contains("machine was not saved"));
}

#[test]
fn machine_add_checks_hc_without_platform_or_remote_herdr_probes() {
    let result = setup_options(
        "new",
        "n\n",
        false,
        false,
        SetupOptions {
            windows: true,
            ..Default::default()
        },
    );
    assert_hc_only(&result, "default");
}

#[test]
fn machine_add_hc_transport_failure_never_falls_back_to_an_installer() {
    let result = setup_options(
        "new",
        "",
        false,
        false,
        SetupOptions {
            windows_transport_failure: true,
            ..Default::default()
        },
    );
    assert!(!result.success, "{}", result.output);
    assert!(result.catalog.is_none());
    assert_eq!(result.prompts, 0);
    assert!(result.actions.is_empty());
    assert_eq!(result.command_order, "hc-connect\n");
    assert!(result.output.contains("Connection refused"));
}

#[test]
fn machine_add_does_not_discover_or_adopt_remote_herdr_sessions() {
    for sessions in [
        "failed",
        "not json",
        r#"{"sessions":[{"name":"agents","running":true}]}"#,
    ] {
        let result = setup_options(
            "new",
            "",
            false,
            false,
            SetupOptions {
                sessions: Some(sessions),
                ..Default::default()
            },
        );
        assert_hc_only(&result, "default");
    }
}

#[test]
fn machine_add_checks_hc_for_scripts_and_keeps_explicit_source_sessions() {
    for (noninteractive, session) in [
        (true, None),
        (true, Some("agents")),
        (false, Some("agents")),
    ] {
        let result = setup_options(
            "new",
            "",
            false,
            false,
            SetupOptions {
                noninteractive,
                explicit_session: session,
                ..Default::default()
            },
        );
        assert_hc_only(&result, session.unwrap_or("default"));
        let assignment = format!("HC_SESSION={}", session.unwrap_or("default"));
        assert!(
            result.start_command.contains(&assignment),
            "{}",
            result.start_command
        );
    }
}

#[test]
fn machine_add_reports_strict_host_key_failure() {
    let result = setup_with_strict_host_key_failure("new", "", false, true);
    assert!(!result.success, "{}", result.output);
    assert!(result.catalog.is_none());
    assert_eq!(result.prompts, 0);
    assert!(result.actions.is_empty());
    assert!(
        result.output.contains("Host key verification failed"),
        "{}",
        result.output
    );
    assert!(
        result
            .ssh_args
            .lines()
            .any(|arg| arg == "StrictHostKeyChecking=yes"),
        "{}",
        result.ssh_args
    );
    assert!(result.output.contains("machine was not saved"));
    assert!(
        result
            .output
            .contains("saved machines use strict host-key checking"),
        "{}",
        result.output
    );
    assert!(!result.output.contains("lost connection to server"));
}

#[test]
fn machine_add_existing_remote_herdr_is_neither_updated_stopped_nor_handed_off() {
    for (installed, answer) in [("old", "y\n"), ("old", "\n"), ("new", "y\n"), ("new", "\n")] {
        assert_hc_only(&setup(installed, answer, false), "default");
    }
}

#[test]
fn machine_add_rejects_incompatible_hc_hello_without_saving() {
    for hello in [
        r#"{"id":1,"result":{"generation":2,"framed":true,"core":{"id":"source","name":"host"},"version":"future","methods":[]}}"#,
        r#"{"id":1,"result":{"generation":1,"framed":false,"core":{"id":"source","name":"host"},"version":"future","methods":[]}}"#,
        r#"{"id":1,"result":{"generation":1,"framed":true,"core":{"id":"","name":"host"},"version":"future","methods":[]}}"#,
        r#"{"id":2,"result":{}}"#,
        "not json",
    ] {
        let result = setup_options(
            "new",
            "",
            false,
            false,
            SetupOptions {
                hello: Some(hello),
                ..Default::default()
            },
        );
        assert!(!result.success, "{}", result.output);
        assert!(result.catalog.is_none());
        assert_eq!(result.source_requests.len(), 1);
        assert_eq!(result.source_requests[0]["method"], "hello");
        assert!(result.actions.is_empty());
        assert_eq!(result.prompts, 0);
        assert!(result.output.contains("machine was not saved"));
    }
}

#[test]
fn machine_setup_explicit_remote_handoff_remains_available_but_restart_fallback_defaults_to_no() {
    let result = setup("new", "\n", true);
    assert_eq!(result.actions, "handoff\n", "{}", result.output);
    assert!(
        result
            .output
            .contains("This stops active remote pane processes"),
        "{}",
        result.output
    );
}
