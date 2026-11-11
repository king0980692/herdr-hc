//! Runs the real saved-machine CLI/controller with SSH routed only to this test's scratch core.
use super::*;
use crate::api::{
    client::{ApiClient, ConnectionTarget},
    schema::Request,
};

struct Presenter {
    root: PathBuf,
    exe: PathBuf,
    path: std::ffi::OsString,
    api: Option<(ApiClient, PathBuf)>,
    owned_pid: Option<u32>,
}
impl Presenter {
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(&self.exe);
        command
            .args(args)
            .env("PATH", &self.path)
            .env("XDG_CONFIG_HOME", self.root.join("c"))
            .env("XDG_STATE_HOME", self.root.join("s"))
            .env("XDG_RUNTIME_DIR", self.root.join("r"))
            .env_remove("HERDR_CONFIG_PATH")
            .env_remove("HERDR_STARTUP_CWD")
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("HERDR_CLIENT_SOCKET_PATH")
            .env_remove("HERDR_SESSION")
            .env_remove("HC_SOCKET")
            .env_remove("HC_TERMINAL_ID")
            .env_remove("HC_TERMINAL_SOCKET")
            .env_remove("HC_SESSION")
            .env_remove(crate::hc::source::PROFILE_ENV)
            .kill_on_drop(true);
        command
    }
    async fn cli(&mut self, args: &[&str]) -> io::Result<Value> {
        let output = timeout(Duration::from_secs(25), self.command(args).output())
            .await
            .map_err(|_| invalid("saved-source CLI timed out"))??;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "saved-source CLI {args:?}: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        if let Some((_, path)) = &self.api {
            let peer = crate::ipc::connect_local_stream_cancellable(
                path,
                &std::sync::atomic::AtomicBool::new(false),
                std::time::Instant::now() + Duration::from_millis(100),
            )?;
            self.owned_pid = Some(
                crate::platform::local_stream_peer_pid_for_test(&peer)
                    .ok_or_else(|| invalid("scratch presentation peer PID unavailable"))?,
            );
        }
        if output.stdout.is_empty() {
            Ok(Value::Null)
        } else {
            Ok(serde_json::from_slice(&output.stdout).unwrap_or(Value::Null))
        }
    }
    async fn stop(&mut self) -> io::Result<()> {
        let Some((client, path)) = &self.api else {
            return Ok(());
        };
        let owned_pid = self.owned_pid;
        if owned_pid.is_none() {
            return Err(invalid(
                "scratch presentation owner is unrecorded; cleanup unconfirmed",
            ));
        }
        let peer = timeout(Duration::from_secs(10), async {
            loop {
                match crate::ipc::connect_local_stream_cancellable(
                    path,
                    &std::sync::atomic::AtomicBool::new(false),
                    std::time::Instant::now() + Duration::from_millis(100),
                ) {
                    Ok(stream) => return Ok(Some(stream)),
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::NotFound
                                | io::ErrorKind::ConnectionRefused
                                | io::ErrorKind::TimedOut
                        ) =>
                    {
                        if matches!(
                            error.kind(),
                            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                        ) && owned_pid.is_some_and(presentation_exited)
                        {
                            return Ok(None);
                        }
                        tokio::time::sleep(Duration::from_millis(25)).await
                    }
                    Err(error) => return Err(error),
                }
            }
        })
        .await
        .map_err(|_| invalid("scratch presentation endpoint unavailable; cleanup unconfirmed"))??;
        let Some(peer) = peer else {
            self.api = None;
            self.owned_pid = None;
            return Ok(());
        };
        let pid = crate::platform::local_stream_peer_pid_for_test(&peer)
            .ok_or_else(|| invalid("scratch presentation peer PID unavailable"))?;
        if owned_pid.is_some_and(|owned| owned != pid) {
            return Err(invalid(
                "scratch presentation owner changed; cleanup unconfirmed",
            ));
        }
        let stopped = (|| {
            let source = request(client, "runtime.source", json!({}))?;
            let pinned = client.clone().with_source_instance(
                source["host_instance"]
                    .as_str()
                    .ok_or_else(|| invalid("no host instance for stop"))?
                    .into(),
            );
            request(&pinned, "server.stop", json!({}))
        })();
        timeout(Duration::from_secs(10), async {
            loop {
                if presentation_exited(pid) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .map_err(|_| {
            io::Error::other(format!(
                "scratch presentation host did not stop: {stopped:?}"
            ))
        })?;
        self.api = None;
        self.owned_pid = None;
        Ok(())
    }
}
fn presentation_exited(pid: u32) -> bool {
    (unsafe { libc::kill(pid as libc::pid_t, 0) }) != 0
        && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}
fn request(client: &ApiClient, method: &str, params: Value) -> io::Result<Value> {
    let request: Request =
        serde_json::from_value(json!({"id":"saved-source-smoke","method":method,"params":params}))
            .map_err(io::Error::other)?;
    let value = client
        .request_value_with_timeout(&request, Duration::from_secs(15))
        .map_err(io::Error::other)?;
    if value.get("error").is_some() {
        return Err(io::Error::other(value.to_string()));
    }
    Ok(value["result"].clone())
}
async fn presenter(target: &Target) -> io::Result<Presenter> {
    // Keep private named-session Unix socket paths below macOS's length limit.
    let name = format!(
        "hp-{:08x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos() as u32
    );
    let root = PathBuf::from("/var/tmp").join(name);
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new().mode(0o700).create(&root)?;
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin)?;
    let exe = std::env::current_exe()?
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| invalid("no build directory"))?
        .join("herdr");
    if !exe.is_file() {
        return Err(invalid("build the herdr CLI before the saved-source smoke"));
    }
    let hc_path = format!("{}/hc source' pinned", target.scratch);
    let alias_body = format!(
        "#!/bin/sh\nexec {} \"$@\"\n",
        crate::remote::shell_quote(&target.executable)
    );
    for path in [&hc_path, &format!("{hc_path}-alias")] {
        let quoted = crate::remote::shell_quote(path);
        let output = source_script(
            target,
            &format!(
                "umask 077; test ! -e {quoted} && printf '%s' {} > {quoted} && chmod u+x {quoted}",
                crate::remote::shell_quote(&alias_body)
            ),
        )
        .await?;
        if !output.status.success() {
            return Err(invalid("could not create private hc executable aliases"));
        }
    }
    let allowed = [
        "exec hc connect".to_owned(),
        format!("exec {} connect", crate::remote::shell_quote(&hc_path)),
        format!(
            "exec {} connect",
            crate::remote::shell_quote(&format!("{hc_path}-alias"))
        ),
    ];
    let patterns = allowed
        .iter()
        .map(|suffix| format!("*{}", crate::remote::shell_quote(suffix)))
        .collect::<Vec<_>>()
        .join("|");
    let prefix = format!(
        "env XDG_CONFIG_HOME='{}' XDG_STATE_HOME='{}' XDG_RUNTIME_DIR='{}'",
        target.scratch, target.scratch, target.scratch
    );
    let forward = if target.host.is_some() {
        format!("printf -v quoted '%q' \"$last\"\nexec /usr/bin/ssh \"${{@:1:$#-1}}\" \"{prefix} /bin/sh -c $quoted\"\n")
    } else {
        format!("exec {prefix} /bin/sh -c \"$last\"\n")
    };
    let shim = bin.join("ssh");
    let gate = crate::remote::shell_quote(
        root.join("block-hc-connect")
            .to_str()
            .ok_or_else(|| invalid("invalid private gate path"))?,
    );
    std::fs::write(&shim,format!("#!/bin/bash\nlast=\"${{!#}}\"\ncase \"$last\" in {patterns}) ;; *) exit 93;; esac\n[[ ! -e {gate} ]] || exit 94\n{forward}"))?;
    if !std::process::Command::new("chmod")
        .arg("u+x")
        .arg(&shim)
        .status()?
        .success()
    {
        return Err(invalid("scratch SSH shim is not executable"));
    }
    let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))
    .map_err(io::Error::other)?;
    let config = root.join("c/herdr-dev");
    std::fs::create_dir_all(&config)?;
    std::fs::write(
        config.join("config.toml"),
        "onboarding = false\n[terminal]\ndefault_shell = \"/bin/sh\"\nshell_mode = \"non_login\"\nnew_cwd = \"home\"\n",
    )?;
    Ok(Presenter {
        root,
        exe,
        path,
        api: None,
        owned_pid: None,
    })
}
async fn exercise(target: &Target, presenter: &mut Presenter) -> io::Result<()> {
    let host = target.host.as_deref().unwrap_or("scratch-hc-source");
    presenter
        .cli(&[
            "machine",
            "add",
            host,
            "--label",
            "scratch-source",
            "--remote-session",
            "native-smoke",
            "--hc-path",
            &format!("{}/hc source' pinned", target.scratch),
        ])
        .await?;
    let path = presenter.root.join("s/herdr-dev/client/endpoints.json");
    let mut catalog: crate::client::endpoint::EndpointCatalog =
        serde_json::from_slice(&std::fs::read(&path)?).map_err(io::Error::other)?;
    assert_eq!(
        catalog.ssh[0].hc_path.as_deref(),
        Some(format!("{}/hc source' pinned", target.scratch).as_str())
    );
    catalog.selected_profile = Some(catalog.ssh[0].id.clone());
    std::fs::write(
        &path,
        serde_json::to_vec(&catalog).map_err(io::Error::other)?,
    )?;
    let profile = &catalog.ssh[0];
    let id = profile.id.as_str();
    let api_path = presenter
        .root
        .join("c/herdr-dev/sessions")
        .join(format!("hc-{id}"))
        .join("herdr.sock");
    let api = ApiClient::for_target(ConnectionTarget::SocketPath(api_path.clone()));
    presenter.api = Some((api.clone(), api_path.clone()));
    let probe = Client::connect(target.command(&["connect"])).await?;
    assert!(live_programs(&probe).await?.is_empty());
    presenter
        .cli(&["--machine", id, "workspace", "list"])
        .await?;
    assert!(
        live_programs(&probe).await?.is_empty(),
        "machine activation implicitly launched a program"
    );
    let info = request(&api, "runtime.source", json!({}))?;
    assert_eq!(info["core_id"], probe.hello.core.id);
    assert_eq!(info["profile_id"], id);
    assert_eq!(info["target"], host);
    assert_eq!(info["hc_path"].as_str(), profile.hc_path.as_deref());
    let old = api
        .clone()
        .with_source_instance(info["host_instance"].as_str().unwrap().into());
    let created = request(&old, "workspace.create", json!({"focus":true}))?;
    let workspace = created["workspace"]["workspace_id"]
        .as_str()
        .ok_or_else(|| invalid("no saved workspace id"))?
        .to_owned();
    let pane = created["root_pane"]["pane_id"]
        .as_str()
        .ok_or_else(|| invalid("no saved pane id"))?
        .to_owned();
    let programs = live_programs(&probe).await?;
    assert_eq!(programs.len(), 1);
    let (_, stream) = probe
        .open(
            "terminal.attach",
            json!({"terminal":programs.keys().next().unwrap(),"cols":80,"rows":24}),
        )
        .await?;
    stream
        .input(
            Bytes::from_static(b"printf '\\033]7501;state=idle:app=codex\\007'\r"),
            false,
        )
        .await?;
    drop(stream);
    timeout(Duration::from_secs(5), async {
        loop {
            let agents = request(&old, "agent.list", json!({}))?;
            if agents["agents"]
                .as_array()
                .is_some_and(|agents| agents.iter().any(|a| a["agent_status"] == "idle"))
            {
                return Ok::<_, io::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .map_err(|_| invalid("source-reported agent was not projected"))??;
    let prompted = request(
        &old,
        "agent.prompt",
        json!({"target":pane,"text":"printf '\\033]7501;state=blocked:app=codex\\007SAVED_READY\\n'","wait":{"until":["blocked"],"timeout_ms":5000}}),
    )?;
    assert_eq!(prompted["type"], "agent_prompted");
    assert_eq!(prompted["agent"]["agent_status"], "blocked");
    let waited = request(
        &old,
        "agent.wait",
        json!({"target":pane,"until":["blocked"],"timeout_ms":1000}),
    )?;
    assert_eq!(waited["agent"]["agent_status"], "blocked");
    bare_tui_reopens_saved_source(presenter, &probe).await?;
    if target.host.is_some() {
        let list = probe.call("terminal.list", json!({})).await?;
        assert!(!list["terminals"][0]["cwd"]
            .as_str()
            .unwrap_or_default()
            .starts_with("/Users/"));
    }
    presenter.stop().await?;
    assert_eq!(
        live_programs(&probe).await?,
        programs,
        "presentation shutdown killed the hc program"
    );
    presenter.api = Some((api.clone(), api_path));
    presenter
        .cli(&["--machine", id, "workspace", "list"])
        .await?;
    let new_info = request(&api, "runtime.source", json!({}))?;
    assert_ne!(new_info["host_instance"], info["host_instance"]);
    let current = api
        .clone()
        .with_source_instance(new_info["host_instance"].as_str().unwrap().into());
    let panes = request(&current, "pane.list", json!({}))?;
    assert_eq!(panes["panes"][0]["pane_id"], pane);
    assert_eq!(
        live_programs(&probe).await?,
        programs,
        "saved-source restart relaunched a program"
    );
    let stale: Request =
        serde_json::from_value(json!({"id":"stale","method":"workspace.create","params":{}}))
            .map_err(io::Error::other)?;
    let stale = old
        .request_value_with_timeout(&stale, Duration::from_secs(2))
        .map_err(io::Error::other)?;
    assert_eq!(stale["error"]["code"], "stale_runtime_source");
    assert_eq!(live_programs(&probe).await?, programs);
    request(
        &current,
        "workspace.close",
        json!({"workspace_id":workspace}),
    )?;
    assert!(live_programs(&probe).await?.is_empty());
    println!("Saved machine: hc-only SSH/local presentation, no implicit launch, default source cwd, detach/restart + PID/public-ID preservation, stale-host rejection, exact close verified");
    exercise_catalog_retirement(presenter, &api, &probe).await?;
    exercise_core_replacement(target, presenter, &api, &probe).await
}

async fn bare_tui_reopens_saved_source(presenter: &Presenter, probe: &Client) -> io::Result<()> {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use std::io::Read as _;
    use std::sync::{Arc, Mutex};
    // A stalled private Local endpoint prevents any unrelated local core from being created.
    let local_path = presenter.root.join("c/herdr-dev/herdr-client.sock");
    let local = std::os::unix::net::UnixListener::bind(&local_path)?;
    let programs = live_programs(probe).await?;
    for _ in 0..2 {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 35,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io::Error::other)?;
        let mut reader = pair.master.try_clone_reader().map_err(io::Error::other)?;
        let template = presenter.command(&[]);
        let mut command = CommandBuilder::new(&presenter.exe);
        for (key, value) in template.as_std().get_envs() {
            match value {
                Some(value) => command.env(key, value),
                None => command.env_remove(key),
            }
        }
        command.env("TERM", "xterm-256color");
        command.cwd(&presenter.root);
        let mut child = pair
            .slave
            .spawn_command(command)
            .map_err(io::Error::other)?;
        drop(pair.slave);
        let output = Arc::new(Mutex::new(Vec::<u8>::new()));
        let collected = output.clone();
        let reader = std::thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            while let Ok(count) = reader.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                let mut bytes = collected.lock().unwrap();
                bytes.extend_from_slice(&buffer[..count]);
                if bytes.len() > 1024 * 1024 {
                    let excess = bytes.len() - 1024 * 1024;
                    bytes.drain(..excess);
                }
            }
        });
        let visible = timeout(Duration::from_secs(12), async {
            loop {
                let shown = {
                    let bytes = output.lock().unwrap();
                    bytes
                        .windows(b"scratch-source".len())
                        .any(|s| s == b"scratch-source")
                        && bytes
                            .windows(b"SAVED_READY".len())
                            .any(|s| s == b"SAVED_READY")
                };
                if shown {
                    return Ok::<_, io::Error>(());
                }
                if child.try_wait().map_err(io::Error::other)?.is_some() {
                    return Err(invalid("bare herdr exited before showing the saved source"));
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await;
        // Kill only this recorded test client: exit/transport loss must detach, never kill source programs.
        let killed = match child.try_wait().map_err(io::Error::other)? {
            Some(_) => Ok(()),
            None => child.kill().map_err(io::Error::other),
        };
        let reaped = child.wait().map_err(io::Error::other);
        drop(pair.master);
        reader
            .join()
            .map_err(|_| invalid("bare TUI output reader panicked"))?;
        killed?;
        reaped?;
        visible.map_err(|_| invalid("bare herdr did not display saved-source output"))??;
        assert_eq!(
            live_programs(probe).await?,
            programs,
            "normal TUI exit/reopen changed source programs or PIDs"
        );
    }
    drop(local);
    std::fs::remove_file(local_path)?;
    println!("Normal flow: machine add --hc-path, persisted selection, bare herdr render, detach and reopen preserve source programs/PIDs");
    Ok(())
}

async fn exercise_catalog_retirement(
    presenter: &mut Presenter,
    api: &ApiClient,
    probe: &Client,
) -> io::Result<()> {
    let path = presenter.root.join("s/herdr-dev/client/endpoints.json");
    let original = std::fs::read(&path)?;
    let baseline: crate::client::endpoint::EndpointCatalog =
        serde_json::from_slice(&original).map_err(io::Error::other)?;
    let id = baseline.ssh[0].id.as_str();
    for change in [
        "retarget",
        "hc-path",
        "hc-path-offline",
        "disable",
        "remove",
    ] {
        let source = request(api, "runtime.source", json!({}))?;
        assert_eq!(source["connected"], true);
        let mut pinned = api.clone().with_source_instance(
            source["host_instance"]
                .as_str()
                .ok_or_else(|| invalid("no source instance"))?
                .into(),
        );
        let created = request(&pinned, "workspace.create", json!({}))?;
        let workspace = created["workspace"]["workspace_id"].clone();
        let pane = created["root_pane"]["pane_id"].clone();
        let programs = live_programs(probe).await?;
        assert_eq!(programs.len(), 1);
        if change == "hc-path-offline" {
            std::fs::write(
                presenter.root.join("block-hc-connect"),
                b"private offline gate",
            )?;
            let api_path = presenter
                .api
                .as_ref()
                .ok_or_else(|| invalid("no presenter endpoint"))?
                .1
                .clone();
            presenter.stop().await?;
            assert_eq!(live_programs(probe).await?, programs);
            presenter.api = Some((api.clone(), api_path));
            presenter
                .cli(&["--machine", id, "workspace", "list"])
                .await?;
            let offline = request(api, "runtime.source", json!({}))?;
            assert_eq!(offline["connected"], false);
            assert_eq!(
                offline["hc_path"].as_str(),
                baseline.ssh[0].hc_path.as_deref()
            );
            pinned = api.clone().with_source_instance(
                offline["host_instance"]
                    .as_str()
                    .ok_or_else(|| invalid("offline host not fenced"))?
                    .into(),
            );
        }
        let mut changed = baseline.clone();
        match change {
            // The SSH shim reaches the same core: a route change still must retire.
            "retarget" => changed.ssh[0].target = "same-core-alias.invalid".into(),
            "hc-path" | "hc-path-offline" => {
                changed.ssh[0].hc_path = Some(format!(
                    "{}-alias",
                    baseline.ssh[0]
                        .hc_path
                        .as_deref()
                        .ok_or_else(|| invalid("no custom hc path"))?
                ))
            }
            "disable" => changed.ssh[0].enabled = false,
            "remove" => changed.ssh.clear(),
            _ => unreachable!(),
        }
        let next = path.with_extension("next");
        std::fs::write(
            &next,
            serde_json::to_vec(&changed).map_err(io::Error::other)?,
        )?;
        std::fs::rename(&next, &path)?;
        let pid = presenter
            .owned_pid
            .ok_or_else(|| invalid("no recorded presenter owner"))?;
        // App retirement deliberately exits this presenter; a retired API need not remain reachable.
        timeout(Duration::from_secs(5), async {
            while !presentation_exited(pid) {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .map_err(|_| {
            let state = std::fs::read_to_string(format!("/proc/{pid}/status"))
                .ok()
                .and_then(|status| {
                    status
                        .lines()
                        .filter(|line| line.starts_with("State:") || line.starts_with("PPid:"))
                        .map(str::to_owned)
                        .reduce(|a, b| format!("{a} {b}"))
                })
                .unwrap_or_else(|| "unavailable".into());
            let log = std::fs::read_to_string(
                presenter.root.join(format!("c/herdr-dev/sessions/hc-{id}/herdr-server.log")),
            )
            .unwrap_or_default();
            let tail: Vec<_> = log.lines().rev().take(12).collect();
            invalid(format!(
                "catalog change did not retire the source host (pid {pid}: {state}); log tail (newest first): {tail:#?}"
            ))
        })?;
        assert_eq!(live_programs(probe).await?, programs);
        let snapshot_path = presenter
            .api
            .as_ref()
            .unwrap()
            .1
            .parent()
            .unwrap()
            .join("session.json");
        let saved: crate::persist::SessionSnapshot =
            serde_json::from_slice(&std::fs::read(snapshot_path)?).map_err(io::Error::other)?;
        assert_eq!(saved.workspaces[0].id.as_deref(), workspace.as_str());
        let bindings: Vec<_> = saved
            .workspaces
            .iter()
            .flat_map(|ws| &ws.tabs)
            .flat_map(|tab| tab.panes.values())
            .filter_map(|pane| pane.hc_binding.as_ref())
            .collect();
        assert_eq!(
            bindings,
            [&json!({"core_id":source["core_id"],"address":programs.keys().next().unwrap()})]
        );
        std::fs::write(&next, &original)?;
        std::fs::rename(&next, &path)?;
        if change == "hc-path-offline" {
            std::fs::remove_file(presenter.root.join("block-hc-connect"))?;
        }
        presenter.stop().await?;
        let api_path = presenter
            .root
            .join("c/herdr-dev/sessions")
            .join(format!("hc-{id}"))
            .join("herdr.sock");
        presenter.api = Some((api.clone(), api_path));
        presenter
            .cli(&["--machine", id, "workspace", "list"])
            .await?;
        let fresh = request(api, "runtime.source", json!({}))?;
        assert_ne!(fresh["host_instance"], source["host_instance"]);
        assert_eq!(fresh["core_id"], source["core_id"]);
        for (method, params) in [
            ("workspace.create", json!({})),
            (
                "pane.send_text",
                json!({"pane_id":pane,"text":"HC_FORBIDDEN_RETIRED_INPUT"}),
            ),
        ] {
            let attempted: Request = serde_json::from_value(
                json!({"id":"retired-action","method":method,"params":params}),
            )
            .map_err(io::Error::other)?;
            let rejected = pinned
                .request_value_with_timeout(&attempted, Duration::from_secs(2))
                .map_err(io::Error::other)?;
            assert_eq!(rejected["error"]["code"], "stale_runtime_source");
        }
        assert_eq!(live_programs(probe).await?, programs);
        let screen = probe
            .call(
                "terminal.read",
                json!({"terminal":programs.keys().next().unwrap()}),
            )
            .await?;
        assert!(!screen["text"]
            .as_str()
            .ok_or_else(|| invalid("no source screen"))?
            .contains("HC_FORBIDDEN_RETIRED_INPUT"));
        assert_eq!(
            request(api, "pane.list", json!({}))?["panes"][0]["pane_id"],
            pane
        );
        assert_eq!(live_programs(probe).await?, programs);
        let current = api.clone().with_source_instance(
            fresh["host_instance"]
                .as_str()
                .ok_or_else(|| invalid("no fresh source instance"))?
                .into(),
        );
        request(
            &current,
            "workspace.close",
            json!({"workspace_id":workspace}),
        )?;
        assert!(live_programs(probe).await?.is_empty());
        println!("Saved catalog {change}: timed retirement, pinned route, no launch/input, retained PID/public ID, attach-only restart, and exact close verified");
    }
    Ok(())
}
async fn source_script(target: &Target, script: &str) -> io::Result<std::process::Output> {
    let mut command = if let Some(host) = &target.host {
        let mut command = ssh(host);
        command.arg(format!("/bin/sh -c {}", crate::remote::shell_quote(script)));
        command
    } else {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    };
    command.kill_on_drop(true);
    timeout(Duration::from_secs(15), command.output())
        .await
        .map_err(|_| invalid("private source script timed out"))?
}

async fn exercise_core_replacement(
    target: &Target,
    presenter: &mut Presenter,
    api: &ApiClient,
    original_core: &Client,
) -> io::Result<()> {
    let source = request(api, "runtime.source", json!({}))?;
    let old_host = api
        .clone()
        .with_source_instance(source["host_instance"].as_str().unwrap().into());
    let created = request(&old_host, "workspace.create", json!({}))?;
    let workspace = created["workspace"]["workspace_id"].clone();
    let pane = created["root_pane"]["pane_id"].clone();
    let initial = request(api, "pane.list", json!({}))?;
    let tab = initial["panes"][0]["tab_id"].clone();
    assert!(tab.as_str().is_some());
    let programs = live_programs(original_core).await?;
    assert_eq!(programs.len(), 1);
    let address = programs.keys().next().unwrap();
    let pid = presenter
        .owned_pid
        .ok_or_else(|| invalid("no owned presenter for core replacement"))?;
    let gate = presenter.root.join("block-hc-connect");
    std::fs::write(
        &gate,
        b"read-only hello paused for private core replacement",
    )?;
    let stopped = timeout(
        Duration::from_secs(15),
        target.command(&["server", "stop", "--json"]).output(),
    )
    .await
    .map_err(|_| invalid("private original core stop timed out"))??;
    assert!(stopped.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&stopped.stdout).map_err(io::Error::other)?["stopped"],
        true
    );
    let root = crate::remote::shell_quote(&target.scratch);
    timeout(Duration::from_secs(5), async {
        loop {
            let check = source_script(
                target,
                &format!("sockets=$(find {root} -type s -print -quit) && test -z \"$sockets\""),
            )
            .await?;
            if check.status.success() {
                return Ok::<_, io::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .map_err(|_| invalid("original private core sockets remain"))??;
    // Only stopped, test-owned state is archived; no user core or upstream source is changed.
    let replaced = source_script(target, &format!("test ! -e {root}/herdr-core/previous-native-smoke && mv -- {root}/herdr-core/native-smoke {root}/herdr-core/previous-native-smoke")).await?;
    assert!(
        replaced.status.success(),
        "private core archive failed: {}",
        String::from_utf8_lossy(&replaced.stderr)
    );
    let replacement = Client::connect(target.command(&["connect"])).await?;
    assert_ne!(replacement.hello.core.id, original_core.hello.core.id);
    assert!(
        live_programs(&replacement).await?.is_empty(),
        "core replacement relaunched an old program"
    );
    let mut same_address = false;
    for _ in 0..64 {
        let run = replacement.call("terminal.run", json!({"argv":["/bin/cat"],"cwd":"/var/tmp","size":{"rows":24,"cols":80},"no_parent":true,"no_notify":true,"keep":false})).await?;
        if run["address"].as_str() == Some(address) {
            same_address = true;
            break;
        }
    }
    assert!(
        same_address,
        "coverage cap: could not occupy the old address within64 private programs"
    );
    let replacement_programs = live_programs(&replacement).await?;
    std::fs::remove_file(gate)?;
    timeout(Duration::from_secs(10), async {
        while !presentation_exited(pid) {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .map_err(|_| invalid("changed core identity did not retire the old presenter"))?;
    assert_eq!(live_programs(&replacement).await?, replacement_programs);
    presenter.stop().await?;
    let catalog: crate::client::endpoint::EndpointCatalog = serde_json::from_slice(&std::fs::read(
        presenter.root.join("s/herdr-dev/client/endpoints.json"),
    )?)
    .map_err(io::Error::other)?;
    let id = catalog.ssh[0].id.as_str();
    let api_path = presenter
        .root
        .join("c/herdr-dev/sessions")
        .join(format!("hc-{id}"))
        .join("herdr.sock");
    let snapshot_path = api_path.parent().unwrap().join("session.json");
    presenter.api = Some((api.clone(), api_path));
    presenter
        .cli(&["--machine", id, "workspace", "list"])
        .await?;
    let current_source = request(api, "runtime.source", json!({}))?;
    assert_eq!(current_source["core_id"], replacement.hello.core.id);
    assert_ne!(current_source["host_instance"], source["host_instance"]);
    let restored = request(api, "pane.list", json!({}))?;
    assert_eq!(restored["panes"][0]["pane_id"], pane);
    assert_eq!(restored["panes"][0]["workspace_id"], workspace);
    assert_eq!(restored["panes"][0]["tab_id"], tab);
    let current = api
        .clone()
        .with_source_instance(current_source["host_instance"].as_str().unwrap().into());
    for client in [&old_host, &current] {
        let attempted: Request = serde_json::from_value(json!({"id":"core-replacement-input","method":"pane.send_text","params":{"pane_id":pane,"text":"HC_FORBIDDEN_REPLACEMENT_INPUT"}})).map_err(io::Error::other)?;
        let response = client
            .request_value_with_timeout(&attempted, Duration::from_secs(2))
            .map_err(io::Error::other)?;
        assert!(
            response.get("error").is_some(),
            "mismatched binding accepted input: {response}"
        );
    }
    let close: Request = serde_json::from_value(json!({"id":"core-replacement-close","method":"workspace.close","params":{"workspace_id":workspace}})).map_err(io::Error::other)?;
    assert!(current
        .request_value_with_timeout(&close, Duration::from_secs(2))
        .map_err(io::Error::other)?
        .get("error")
        .is_some());
    assert_eq!(
        request(api, "pane.list", json!({}))?["panes"][0]["pane_id"],
        pane
    );
    assert_eq!(live_programs(&replacement).await?, replacement_programs);
    let screen = replacement
        .call("terminal.read", json!({"terminal":address}))
        .await?;
    assert!(!screen["text"]
        .as_str()
        .unwrap()
        .contains("HC_FORBIDDEN_REPLACEMENT_INPUT"));
    presenter.stop().await?;
    let saved: crate::persist::SessionSnapshot =
        serde_json::from_slice(&std::fs::read(snapshot_path)?).map_err(io::Error::other)?;
    let bindings: Vec<_> = saved
        .workspaces
        .iter()
        .flat_map(|ws| &ws.tabs)
        .flat_map(|tab| tab.panes.values())
        .filter_map(|pane| pane.hc_binding.as_ref())
        .collect();
    assert_eq!(
        bindings,
        [&json!({"core_id":original_core.hello.core.id,"address":address})]
    );
    assert_eq!(live_programs(&replacement).await?, replacement_programs);
    println!("Saved core replacement: changed permanent ID at the same route/address, timed old-host retirement, retained unavailable binding/public ID, no input/close/relaunch on replacement, and detach-only shutdown verified");
    Ok(())
}

#[tokio::test]
async fn hc_saved_smoke_cleanup_rejects_unrecorded_or_replaced_presentation_owners() {
    use interprocess::local_socket::traits::Listener as _;
    use std::{io::Read as _, os::unix::fs::DirBuilderExt as _};
    let root = PathBuf::from("/var/tmp").join(format!(
        "ho-{}-{:x}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&root)
        .unwrap();
    let path = root.join("api.sock");
    let listener = crate::ipc::bind_private_local_listener(&path).unwrap();
    let mut presenter = Presenter {
        root: root.clone(),
        exe: "/must-not-spawn".into(),
        path: "".into(),
        api: Some((
            ApiClient::for_target(ConnectionTarget::SocketPath(path.clone())),
            path,
        )),
        owned_pid: Some(std::process::id() + 1),
    };
    let error = presenter
        .stop()
        .await
        .expect_err("replacement owner was accepted");
    assert!(error.to_string().contains("owner changed"));
    assert!(
        presenter.api.is_some(),
        "uncertain ownership evidence was discarded"
    );
    let mut peer = listener.accept().unwrap();
    let mut byte = [0];
    assert_eq!(
        peer.read(&mut byte).unwrap(),
        0,
        "cleanup sent a request to the replacement"
    );
    drop(peer);
    presenter.owned_pid = None;
    let error = presenter
        .stop()
        .await
        .expect_err("unrecorded owner was accepted");
    assert!(error.to_string().contains("owner is unrecorded"));
    assert!(presenter.api.is_some());
    listener
        .set_nonblocking(interprocess::local_socket::ListenerNonblockingMode::Accept)
        .unwrap();
    match listener.accept() {
        Err(error) => assert_eq!(error.kind(), io::ErrorKind::WouldBlock),
        Ok(_) => panic!("cleanup connected without a recorded owner"),
    }
    drop(listener);
    std::fs::remove_dir_all(root).unwrap();
}

pub(super) fn run(target: &mut Target, runtime: &tokio::runtime::Runtime) -> io::Result<()> {
    let mut presenter = runtime.block_on(presenter(target))?;
    target.presentation_pending = Some(presenter.root.clone());
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(exercise(target, &mut presenter))
    }));
    if let Ok(Err(error)) = &outcome {
        eprintln!("saved-source exercise failed: {error}");
    }
    let stopped = runtime.block_on(presenter.stop());
    if stopped.is_ok() {
        target.presentation_pending = None;
        std::fs::remove_dir_all(&presenter.root)?;
    }
    match outcome {
        Ok(outcome) => {
            stopped?;
            outcome
        }
        Err(panic) => std::panic::resume_unwind(panic),
    }
}
