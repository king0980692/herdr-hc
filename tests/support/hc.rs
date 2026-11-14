//! Real hc fixtures: each Herdr test runtime gets a private core, including across presenter restarts.
use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    fs,
    io::{self, Read as _},
    os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

static CORES: OnceLock<Mutex<HashMap<PathBuf, PathBuf>>> = OnceLock::new();

fn quote(path: &Path) -> String {
    format!(
        "'{}'",
        path.to_str()
            .expect("fixture paths must be UTF-8")
            .replace('\'', "'\\''")
    )
}

fn core_root(runtime: &Path) -> PathBuf {
    let cores = CORES.get_or_init(Default::default);
    if let Some(root) = cores.lock().unwrap().get(runtime).cloned() {
        return root;
    }
    let executable = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("hc"))
        .find(|path| path.is_file())
        .and_then(|path| path.canonicalize().ok())
        .expect("hc is required for subprocess runtime tests; install upstream hc on PATH");
    // Keep even a 64-byte session's holder socket within macOS's 103-byte limit.
    let root = loop {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = PathBuf::from(format!("/var/tmp/h{:07x}", nonce & 0xfffffff));
        match fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => break path,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!("create scratch hc: {error}"),
        }
    };
    fs::create_dir(root.join("bin")).unwrap();
    fs::create_dir(root.join("home")).unwrap();
    fs::create_dir_all(root.join("config/herdr-core")).unwrap();
    fs::write(
        root.join("config/herdr-core/config.toml"),
        "notes = false\n",
    )
    .unwrap();
    let script = format!(
        "#!/bin/sh\nexport XDG_CONFIG_HOME={config} XDG_STATE_HOME={state} XDG_RUNTIME_DIR={root}\nexport HC_SESSION=\"${{HC_SESSION:-fixture}}\"\nexport HC_SOCKET={root}/\"$HC_SESSION\".sock\nunset HC_TERMINAL_ID HC_TERMINAL_SOCKET HERDR_HC_SOURCE_PROFILE\nexec {executable} \"$@\"\n",
        config = quote(&root.join("config")),
        state = quote(&root.join("state")),
        root = quote(&root),
        executable = quote(&executable),
    );
    let wrapper = root.join("bin/hc");
    fs::write(&wrapper, script).unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    let mut cores = cores.lock().unwrap();
    if let Some(existing) = cores.get(runtime) {
        let _ = fs::remove_dir_all(&root);
        return existing.clone();
    }
    cores.insert(runtime.to_path_buf(), root.clone());
    root
}

pub fn private_cli(runtime: &Path) -> PathBuf {
    super::register_runtime_dir(runtime);
    core_root(runtime).join("bin/hc")
}

fn path_with_core(root: &Path, inherited: Option<OsString>) -> OsString {
    std::env::join_paths(
        std::iter::once(root.join("bin"))
            .chain(std::env::split_paths(&inherited.unwrap_or_default())),
    )
    .unwrap()
}

pub fn configure_pty(command: &mut portable_pty::CommandBuilder, runtime: &Path) {
    super::register_runtime_dir(runtime);
    let root = core_root(runtime);
    command.env(
        "PATH",
        path_with_core(&root, command.get_env("PATH").map(OsString::from)),
    );
    if !command
        .iter_extra_env_as_str()
        .any(|(key, _)| key == "HOME")
    {
        command.env("HOME", root.join("home"));
    }
    if !command
        .iter_extra_env_as_str()
        .any(|(key, _)| key == "XDG_STATE_HOME")
    {
        command.env("XDG_STATE_HOME", runtime.join("state"));
    }
    if !command
        .iter_extra_env_as_str()
        .any(|(key, _)| key == "HERDR_CONFIG_PATH")
    {
        command.env_remove("HERDR_CONFIG_PATH");
    }
    command.env("HC_SESSION", "fixture");
    for key in [
        "HC_SOCKET",
        "HC_TERMINAL_ID",
        "HC_TERMINAL_SOCKET",
        "HERDR_HC_SOURCE_PROFILE",
    ] {
        command.env_remove(key);
    }
}

pub fn configure(command: &mut Command, runtime: &Path) {
    super::register_runtime_dir(runtime);
    let root = core_root(runtime);
    let path = command
        .get_envs()
        .find(|(key, _)| *key == "PATH")
        .map(|(_, value)| value.map(OsString::from))
        .unwrap_or_else(|| std::env::var_os("PATH"));
    command.env("PATH", path_with_core(&root, path));
    if !command.get_envs().any(|(key, _)| key == "HOME") {
        command.env("HOME", root.join("home"));
    }
    if !command.get_envs().any(|(key, _)| key == "XDG_STATE_HOME") {
        command.env("XDG_STATE_HOME", runtime.join("state"));
    }
    if !command
        .get_envs()
        .any(|(key, _)| key == "HERDR_CONFIG_PATH")
    {
        command.env_remove("HERDR_CONFIG_PATH");
    }
    command.env("HC_SESSION", "fixture");
    for key in [
        "HC_SOCKET",
        "HC_TERMINAL_ID",
        "HC_TERMINAL_SOCKET",
        "HERDR_HC_SOURCE_PROFILE",
    ] {
        command.env_remove(key);
    }
}

fn source_json(runtime: &Path, args: &[&str]) -> serde_json::Value {
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", "exec hc \"$@\"", "hc-fixture"])
        .args(args);
    configure(&mut command, runtime);
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "scratch hc {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("scratch hc JSON")
}

/// A terminal's screen as text.
pub fn screen(runtime: &Path, address: &str) -> String {
    source_json(runtime, &["read", address, "--json"])["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

pub fn terminals(runtime: &Path) -> Vec<serde_json::Value> {
    // `ls` deliberately omits PID; `get` is the complete source record.
    source_json(runtime, &["ls", "--all", "--json"])["terminals"]
        .as_array()
        .expect("scratch hc terminal list")
        .iter()
        .map(|terminal| {
            source_json(
                runtime,
                &["get", terminal["address"].as_str().unwrap(), "--json"],
            )
        })
        .collect()
}

pub fn request(socket: &Path, method: &str, params: serde_json::Value) -> serde_json::Value {
    use std::io::{BufRead, Write};
    let mut stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    writeln!(
        stream,
        "{}",
        serde_json::json!({"id":"hc-fixture","method":method,"params":params})
    )
    .unwrap();
    let mut line = String::new();
    std::io::BufReader::new(stream)
        .read_line(&mut line)
        .unwrap();
    serde_json::from_str(&line).unwrap()
}

static STATUS_AGENTS: OnceLock<Mutex<HashSet<(PathBuf, String)>>> = OnceLock::new();

/// A stand-in agent that owns the pane's foreground and reports whatever state
/// it is told on stdin. hc clears a status once the pane's interactive shell is
/// back in the foreground, so a report typed at the prompt would not last.
const STATUS_AGENT: &str = r#"emit() { printf "\033]7501;state=%s:app=%s\007" "$1" "$2"; }; emit "$1" "$2"; while IFS= read -r line; do set -- $line; case "$1" in clear) printf "\033]7501;state=clear\007"; exit 0 ;; idle|working|blocked|done|error) emit "$1" "$2" ;; esac; done"#;

fn send_status_line(socket: &Path, pane: &str, text: String) {
    let response = request(
        socket,
        "pane.send_text",
        serde_json::json!({"pane_id":pane,"text":text}),
    );
    assert!(
        response.get("error").is_none(),
        "emit source status: {response}"
    );
}

/// Makes the pane's program report `agent` in `state`, as a real agent does on
/// its own terminal. The first report starts the stand-in agent in the pane's
/// shell; later ones drive it, so the pane is no longer an idle shell.
pub fn report_agent(socket: &Path, pane: &str, agent: &str, state: &str) {
    assert!([agent, state].iter().all(|value| !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))));
    let started = STATUS_AGENTS
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .insert((socket.to_path_buf(), pane.to_owned()));
    send_status_line(
        socket,
        pane,
        if started {
            format!("/bin/sh -c '{STATUS_AGENT}' hc-status-agent {state} {agent}\n")
        } else {
            format!("{state} {agent}\n")
        },
    );
    let mut last = serde_json::Value::Null;
    assert!(
        super::wait_until(Duration::from_secs(5), Duration::from_millis(25), || {
            last = request(socket, "pane.get", serde_json::json!({"pane_id":pane}));
            last["result"]["pane"]["agent_status"] == state
                && last["result"]["pane"]["agent"] == agent
        }),
        "source status {agent}/{state} not projected: {last}"
    );
}

/// Withdraws the pane program's root status record, as a program does on exit,
/// and returns the pane to its shell.
pub fn clear_agent(socket: &Path, pane: &str) {
    let running = STATUS_AGENTS
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .remove(&(socket.to_path_buf(), pane.to_owned()));
    send_status_line(
        socket,
        pane,
        if running {
            "clear\n".to_owned()
        } else {
            "printf '\\033]7501;state=clear\\007'\n".to_owned()
        },
    );
    let mut last = serde_json::Value::Null;
    assert!(
        super::wait_until(Duration::from_secs(5), Duration::from_millis(25), || {
            last = request(socket, "pane.get", serde_json::json!({"pane_id":pane}));
            last["result"]["pane"]["agent_status"] == "unknown"
        }),
        "source status clear not projected: {last}"
    );
}

pub fn cleanup(runtime: &Path) -> io::Result<()> {
    let Some(cores) = CORES.get() else {
        return Ok(());
    };
    let root = cores.lock().unwrap().get(runtime).cloned();
    let Some(root) = root else {
        return Ok(());
    };
    cleanup_root(&root).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "scratch hc cleanup failed; retained {}: {error}",
                root.display()
            ),
        )
    })?;
    cores.lock().unwrap().remove(runtime);
    Ok(())
}

fn cleanup_root(root: &Path) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut confirmed = HashSet::new();
    loop {
        let mut unconfirmed = false;
        let sessions = match fs::read_dir(root.join("state/herdr-core")) {
            Ok(entries) => Some(entries),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        for entry in sessions.into_iter().flatten() {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            // `server stop` connects only to an existing core; never start one to clean up.
            let mut child = Command::new(root.join("bin/hc"))
                .args(["server", "stop", "--json"])
                .env("HC_SESSION", entry.file_name())
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()?;
            let status = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break Ok(status),
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(20))
                    }
                    result => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break Err(result.err().unwrap_or_else(|| {
                            io::Error::new(io::ErrorKind::TimedOut, "hc stop timed out")
                        }));
                    }
                }
            }?;
            if !status.success() {
                return Err(io::Error::other(format!("hc stop failed: {status}")));
            }
            let mut output = String::new();
            child
                .stdout
                .take()
                .expect("piped hc stdout")
                .read_to_string(&mut output)?;
            let stopped: serde_json::Value = serde_json::from_str(&output)?;
            match stopped["stopped"].as_bool() {
                Some(true) => {
                    confirmed.insert(entry.file_name());
                }
                // A core writes its identity (core.json) before it serves anyone. Without
                // one it never started: a launcher stopped mid-start (macOS hands launchd
                // jobs their state over a socket) leaves nothing that could hold a program.
                Some(false) => {
                    unconfirmed |= !confirmed.contains(&entry.file_name())
                        && entry.path().join("core.json").exists()
                }
                None => return Err(io::Error::other("hc stop did not confirm its outcome")),
            }
        }
        let mut live = false;
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            match entry.file_type() {
                Ok(kind) if kind.is_socket() => {}
                Ok(_) => continue,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            }
            use interprocess::local_socket::{ConnectOptions, GenericFilePath, ToFsName as _};
            match ConnectOptions::new()
                .name(entry.path().to_fs_name::<GenericFilePath>()?)
                .wait_mode(interprocess::ConnectWaitMode::Timeout(
                    Duration::from_millis(100),
                ))
                .connect_sync()
            {
                Ok(_) => live = true,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                    ) => {}
                Err(error) => return Err(error),
            }
        }
        if !live {
            if unconfirmed {
                return Err(io::Error::other(
                    "hc stop was not confirmed; no reachable holder can recover it",
                ));
            }
            return fs::remove_dir_all(root);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "hc core or holder still reachable",
            ));
        }
        // A holder may be recovering a crashed core; retain its root until stop reaches it.
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub fn cleanup_all() -> io::Result<()> {
    let Some(cores) = CORES.get() else {
        return Ok(());
    };
    let runtimes: Vec<_> = cores.lock().unwrap().keys().cloned().collect();
    let mut result = Ok(());
    for runtime in runtimes {
        if let Err(error) = cleanup(&runtime) {
            eprintln!("hc fixture cleanup: {error}");
            result = Err(error);
        }
    }
    result
}
