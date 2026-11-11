//! Opt-in checks against the installed upstream core, isolated from user state.
mod multiview;
mod saved;

use super::*;
use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

struct Target {
    host: Option<String>,
    executable: String,
    scratch: String,
    presentation_pending: Option<PathBuf>,
}

impl Target {
    fn command(&self, args: &[&str]) -> Command {
        if let Some(host) = &self.host {
            let mut argv = vec![
                "env".to_owned(),
                "-u".into(),
                "HC_SOCKET".into(),
                "-u".into(),
                "HC_TERMINAL_ID".into(),
                "-u".into(),
                "HC_TERMINAL_SOCKET".into(),
                "HC_SESSION=native-smoke".into(),
                format!("XDG_CONFIG_HOME={}", self.scratch),
                format!("XDG_STATE_HOME={}", self.scratch),
                format!("XDG_RUNTIME_DIR={}", self.scratch),
                self.executable.clone(),
            ];
            argv.extend(args.iter().map(|s| (*s).to_owned()));
            let remote = argv
                .iter()
                .map(|arg| format!("'{}'", arg.replace('\'', "'\"'\"'")))
                .collect::<Vec<_>>()
                .join(" ");
            let mut command = ssh(host);
            command.arg(remote).kill_on_drop(true);
            command
        } else {
            let mut command = Command::new(&self.executable);
            command
                .args(args)
                .env_remove("HC_SOCKET")
                .env_remove("HC_TERMINAL_ID")
                .env_remove("HC_TERMINAL_SOCKET")
                .env("HC_SESSION", "native-smoke")
                .env("XDG_CONFIG_HOME", &self.scratch)
                .env("XDG_STATE_HOME", &self.scratch)
                .env("XDG_RUNTIME_DIR", &self.scratch)
                .kill_on_drop(true);
            command
        }
    }

    async fn cleanup(&self) -> io::Result<()> {
        if let Some(root) = &self.presentation_pending {
            return Err(io::Error::other(format!(
                "presentation cleanup unconfirmed; retained {} and {}",
                root.display(),
                self.scratch
            )));
        }
        timeout(Duration::from_secs(15), self.cleanup_source())
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "scratch core cleanup timed out")
            })?
    }

    async fn cleanup_source(&self) -> io::Result<()> {
        let stopped = timeout(
            Duration::from_secs(15),
            self.command(&["server", "stop", "--json"]).output(),
        )
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "scratch core cleanup timed out"))??;
        if !stopped.status.success()
            || serde_json::from_slice::<Value>(&stopped.stdout).map_err(io::Error::other)?
                ["stopped"]
                != true
        {
            return Err(io::Error::other("scratch core stop was not confirmed"));
        }
        if let Some(host) = &self.host {
            // Only remove the exact mktemp directory created by this test.
            if !self.scratch.starts_with("/var/tmp/hn-") || self.scratch[9..].contains('/') {
                return Err(invalid("unsafe scratch cleanup path"));
            }
            let mut command = ssh(host);
            command.arg(format!(
                "sockets=$(find '{}' -type s -print -quit) && test -z \"$sockets\" && rm -rf -- '{}'",
                self.scratch, self.scratch
            ));
            let removed = timeout(Duration::from_secs(15), command.output())
                .await
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::TimedOut, "scratch removal timed out")
                })??;
            if !removed.status.success() {
                return Err(io::Error::other("scratch removal failed"));
            }
        } else {
            use std::os::unix::fs::FileTypeExt as _;
            let mut directories = vec![PathBuf::from(&self.scratch)];
            while let Some(directory) = directories.pop() {
                for entry in std::fs::read_dir(directory)? {
                    let entry = entry?;
                    let is_socket = match entry.file_type() {
                        Ok(kind) if kind.is_dir() => {
                            directories.push(entry.path());
                            continue;
                        }
                        Ok(kind) => kind.is_socket(),
                        Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                        Err(error) => return Err(error),
                    };
                    if is_socket {
                        match crate::ipc::connect_local_stream_cancellable(
                            &entry.path(),
                            &std::sync::atomic::AtomicBool::new(false),
                            std::time::Instant::now() + Duration::from_millis(100),
                        ) {
                            Ok(_) => {
                                return Err(io::Error::other(
                                    "scratch core or holder is still reachable",
                                ))
                            }
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                                ) => {}
                            Err(error) => return Err(error),
                        }
                    }
                }
            }
            std::fs::remove_dir_all(&self.scratch)?;
        }
        Ok(())
    }
}

fn ssh(host: &str) -> Command {
    let mut command = Command::new("ssh");
    command.args([
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=10",
        "--",
        host,
    ]);
    command.kill_on_drop(true);
    command
}

async fn target() -> io::Result<Target> {
    let host = std::env::var("HERDR_HC_SMOKE_HOST").ok();
    let executable = std::env::var("HERDR_HC_SMOKE_EXE").unwrap_or_else(|_| "hc".to_owned());
    let scratch = if let Some(host) = &host {
        let mut command = ssh(host);
        command.arg("mktemp -d /var/tmp/hn-XXXXXX");
        let output = timeout(Duration::from_secs(15), command.output())
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "remote scratch creation timed out")
            })??;
        if !output.status.success() {
            return Err(io::Error::other("remote scratch creation failed"));
        }
        let path = String::from_utf8(output.stdout)
            .map_err(|_| invalid("scratch path is not UTF-8"))?
            .trim()
            .to_owned();
        if !path.starts_with("/var/tmp/hn-")
            || !path
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b))
            || path[9..].contains('/')
        {
            return Err(invalid("unexpected scratch path"));
        }
        path
    } else {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        let path = PathBuf::from("/var/tmp").join(format!("hn-{}-{nanos:x}", std::process::id()));
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new().mode(0o700).create(&path)?;
        path.to_string_lossy().into_owned()
    };
    Ok(Target {
        host,
        executable,
        scratch,
        presentation_pending: None,
    })
}

/// Permanent IDs of cores that are in use, comma-separated. The scratch core's
/// first hello must match none of them before anything is run, attached or killed.
const PROTECTED_CORES_ENV: &str = "HERDR_HC_SMOKE_PROTECTED_CORES";

fn refuse_protected_core(core_id: &str, protected: Option<&str>) -> io::Result<()> {
    if core_id.trim().is_empty() {
        return Err(invalid("scratch core reported no permanent id"));
    }
    if protected
        .into_iter()
        .flat_map(|ids| ids.split(','))
        .any(|id| id.trim() == core_id)
    {
        return Err(invalid(
            "scratch target resolved to a protected core; nothing was run",
        ));
    }
    Ok(())
}

#[test]
fn hc_smoke_refuses_a_protected_core_before_running_anything() {
    assert!(refuse_protected_core("scratch", None).is_ok());
    assert!(refuse_protected_core("scratch", Some("owner-a, owner-b")).is_ok());
    assert!(refuse_protected_core("owner-b", Some("owner-a, owner-b")).is_err());
    assert!(refuse_protected_core(" ", None).is_err());
}

async fn exercise(target: &Target) -> io::Result<()> {
    let client = Client::connect(target.command(&["connect"])).await?;
    refuse_protected_core(
        &client.hello.core.id,
        std::env::var(PROTECTED_CORES_ENV).ok().as_deref(),
    )?;
    println!(
        "native hc: {} ({})",
        client.hello.version, client.hello.core.name
    );
    let (snapshot, mut events) = client.subscribe(json!({})).await?;
    if !snapshot.terminals.is_empty() {
        return Err(invalid("scratch core was not empty"));
    }
    let mut projection = state::Projection::default();
    projection.begin(client.hello.core.id.clone(), 1);
    projection.snapshot(1, snapshot)?;
    let run = client
        .call(
            "terminal.run",
            json!({"argv":["/bin/cat"],"cwd":"/var/tmp",
        "size":{"cols":80,"rows":24},"no_parent":true,"no_notify":true}),
        )
        .await?;
    let address = run
        .get("address")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("run has no address"))?
        .to_owned();
    let (_, mut stream) = client
        .open(
            "terminal.attach",
            json!({"terminal":address,"cols":80,"rows":24}),
        )
        .await?;
    stream.focus(true).await?;
    stream
        .input(Bytes::from_static(b"HC_NATIVE_INPUT\r"), false)
        .await?;
    timeout(Duration::from_secs(10), async {
        let mut output = Vec::new();
        loop {
            let frame = stream.recv().await?;
            if frame.kind == DATA {
                output.extend_from_slice(&frame.payload);
                stream.consumed(frame.payload.len()).await?;
                if output
                    .windows(b"HC_NATIVE_INPUT".len())
                    .any(|part| part == b"HC_NATIVE_INPUT")
                {
                    return Ok::<_, io::Error>(());
                }
                if output.len() > wire::MAX_PAYLOAD {
                    return Err(invalid("unexpected smoke output size"));
                }
            } else if frame.kind == CLOSE {
                return Err(disconnected());
            }
        }
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "native input was not echoed"))??;
    stream.resize(90, 30).await?;
    timeout(Duration::from_secs(10), async {
        loop {
            projection.notification(1, events.next().await?)?;
            if projection.terminals.get(&address).is_some_and(|t| {
                t.facts.get("cols") == Some(&json!(90)) && t.facts.get("rows") == Some(&json!(30))
            }) {
                return Ok::<_, io::Error>(());
            }
        }
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "native resize event missing"))??;
    drop(stream);
    let list = client.call("terminal.list", json!({})).await?;
    if !list["terminals"].as_array().is_some_and(|terminals| {
        terminals
            .iter()
            .any(|t| t["address"] == address && t["state"] == "live")
    }) {
        return Err(invalid("dropping a native attachment ended its program"));
    }
    client
        .call("terminal.kill", json!({"terminal":address}))
        .await?;
    timeout(Duration::from_secs(10), async {
        loop {
            projection.notification(1, events.next().await?)?;
            if projection
                .terminals
                .get(&address)
                .is_some_and(|t| t.facts.get("state") == Some(&json!("finished")))
            {
                return Ok::<_, io::Error>(());
            }
        }
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "native finished event missing"))??;
    Ok(())
}

async fn exercise_runtime(target: &Target) -> io::Result<()> {
    use crate::{layout::PaneId, render_signal::RenderSignal, terminal::TerminalRuntime};
    let client = Client::connect(target.command(&["connect"])).await?;
    let run = client.call("terminal.run", json!({
        "argv":["/bin/sh","-c","printf '\\033]7501;state=blocked\\007HC_RUNTIME_READY\\n'; exec cat"],
        "cwd":"/var/tmp", "size":{"cols":80,"rows":12}, "no_parent":true, "no_notify":true
    })).await?;
    let address = run
        .get("address")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("run has no address"))?
        .to_owned();
    let (events, _receive) = mpsc::channel(16);
    let notify = Arc::new(tokio::sync::Notify::new());
    let dirty = Arc::new(RenderSignal::new());
    let runtime = TerminalRuntime::attach_hc(
        PaneId::from_raw(1),
        12,
        80,
        client.clone(),
        &address,
        0,
        4096,
        crate::terminal_theme::TerminalTheme::default(),
        None,
        events,
        notify.clone(),
        dirty.clone(),
        true,
    )
    .await?;
    timeout(Duration::from_secs(10), async {
        loop {
            dirty.take(); // Simulate the presentation host consuming coalesced damage.
            if runtime.visible_text().contains("HC_RUNTIME_READY") {
                break;
            }
            notify.notified().await;
        }
    })
    .await
    .map_err(|_| invalid("hc output never reached TerminalRuntime parser"))?;
    runtime
        .try_send_bytes(Bytes::from_static(b"HC_RUNTIME_INPUT\r"))
        .map_err(io::Error::other)?;
    timeout(Duration::from_secs(10), async {
        loop {
            dirty.take();
            if runtime.visible_text().contains("HC_RUNTIME_INPUT") {
                break;
            }
            notify.notified().await;
        }
    })
    .await
    .map_err(|_| invalid("TerminalRuntime input was not echoed"))?;
    let mut updates = runtime
        .hc_updates()
        .ok_or_else(|| invalid("missing native runtime updates"))?;
    runtime.resize(18, 90, 9, 18);
    timeout(Duration::from_secs(10), async {
        loop {
            let ready = {
                let state = updates.borrow_and_update();
                state.terminal.facts.get("cols") == Some(&json!(90))
                    && state.terminal.facts.get("rows") == Some(&json!(18))
                    && state
                        .terminal
                        .facts
                        .get("status")
                        .and_then(|s| s.get("state"))
                        == Some(&json!("blocked"))
            };
            if ready {
                return Ok::<_, io::Error>(());
            }
            updates.changed().await.map_err(|_| disconnected())?;
        }
    })
    .await
    .map_err(|_| invalid("TerminalRuntime did not receive authoritative size/status"))??;
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 18)).unwrap();
    terminal
        .draw(|frame| runtime.render(frame, ratatui::layout::Rect::new(0, 0, 90, 18), true))
        .unwrap();
    if runtime.child_pid().is_some() {
        return Err(invalid("hc PID leaked into local runtime ownership"));
    }
    runtime.try_send_focus_event(crate::ghostty::FocusEvent::Gained);
    runtime.terminate_hc().await?;
    drop(runtime);
    let list = client.call("terminal.list", json!({"all":true})).await?;
    if list["terminals"].as_array().is_some_and(|records| {
        records
            .iter()
            .any(|r| r["address"] == address && r["state"] == "live")
    }) {
        return Err(invalid("explicit runtime kill left its terminal live"));
    }
    println!("TerminalRuntime: parser/render/input/resize/status/explicit kill verified");
    Ok(())
}

fn app_request(app: &mut crate::app::App, method: &str, params: Value) -> io::Result<Value> {
    let request =
        serde_json::from_value(json!({"id":"native-app","method":method,"params":params}))
            .map_err(io::Error::other)?;
    let response: Value =
        serde_json::from_str(&app.handle_api_request(request)).map_err(io::Error::other)?;
    if let Some(error) = response.get("error") {
        return Err(io::Error::other(error.to_string()));
    }
    Ok(response)
}

fn app_bindings(app: &crate::app::App) -> std::collections::BTreeMap<String, Value> {
    let mut bindings = std::collections::BTreeMap::new();
    for (ws, workspace) in app.state.workspaces.iter().enumerate() {
        for tab in &workspace.tabs {
            for (pane, state) in &tab.panes {
                bindings.insert(
                    app.public_pane_id(ws, *pane).unwrap(),
                    app.state.terminals[&state.attached_terminal_id]
                        .hc_binding
                        .clone()
                        .unwrap(),
                );
            }
        }
    }
    bindings
}

async fn live_programs(client: &Client) -> io::Result<std::collections::BTreeMap<String, Value>> {
    let list = client.call("terminal.list", json!({"all":true})).await?;
    let records = list["terminals"]
        .as_array()
        .ok_or_else(|| invalid("invalid terminal list"))?;
    Ok(records
        .iter()
        .filter(|record| record["state"] == "live")
        .map(|record| {
            (
                record["address"].as_str().unwrap().to_owned(),
                record["pid"].clone(),
            )
        })
        .collect())
}

async fn exercise_app(target: &Target) -> io::Result<()> {
    use crate::{
        app::{App, AppPolicy},
        terminal::TerminalFactory,
    };
    let probe = Client::connect(target.command(&["connect"])).await?;
    let mut config = crate::config::Config::default();
    config.terminal.default_shell = "/bin/sh".into();
    config.terminal.shell_mode = crate::config::ShellModeConfig::NonLogin;
    let mut app = App::try_new_with_terminal_factory(
        &config,
        AppPolicy::TEST,
        None,
        tokio::sync::mpsc::unbounded_channel().1,
        crate::api::EventHub::default(),
        TerminalFactory::connect(target.command(&["connect"])),
    )?;
    let ws = app.create_workspace_with_options("/var/tmp".into(), true)?;
    let root = app.state.workspaces[ws].tabs[0].root_pane;
    let id = app.state.workspaces[ws].terminal_id(root).unwrap().clone();
    let public_root = app.public_pane_id(ws, root).unwrap();
    let public_workspace = app.public_workspace_id(ws);
    assert_eq!(
        app.state.terminals[&id].state,
        crate::detect::AgentState::Unknown
    );
    let root_address = app.state.terminals[&id].hc_binding.as_ref().unwrap()["address"]
        .as_str()
        .unwrap()
        .to_owned();
    app.terminal_runtimes
        .get(&id)
        .unwrap()
        // hc clears a status once the pane's interactive shell is back in the
        // foreground, so the report comes from a child that keeps running and
        // still runs each later line as a command.
        .try_send_bytes(Bytes::from_static(
            b"/bin/sh -c 'printf \"\\033]7501;state=blocked\\007HC_APP_READY\\n\"; while IFS= read -r l; do eval \"$l\"; done'\r",
        ))
        .map_err(io::Error::other)?;
    timeout(Duration::from_secs(10), async {
        while app.state.terminals[&id].state != crate::detect::AgentState::Blocked {
            let event = app.event_rx.recv().await.ok_or_else(disconnected)?;
            app.handle_internal_event_with_render_impact(event);
        }
        Ok::<_, io::Error>(())
    })
    .await
    .map_err(|_| invalid("App did not project hc status"))??;
    app_request(
        &mut app,
        "pane.split",
        json!({"target_pane_id":public_root,"direction":"right","focus":false}),
    )?;
    app_request(
        &mut app,
        "tab.create",
        json!({"workspace_id":public_workspace,"focus":false}),
    )?;
    app.spawn_popup_argv_command(
        &["/bin/cat".into()],
        Some("/var/tmp".into()),
        Vec::new(),
        Default::default(),
    )?;
    assert!(app.close_popup_pane()?);
    app.state.assert_invariants_for_test();
    assert!(app
        .terminal_runtimes
        .values()
        .all(|runtime| runtime.hc_snapshot().is_some() && runtime.child_pid().is_none()));
    let bindings = app_bindings(&app);
    let before_reconnect = live_programs(&probe).await?;
    let old_tokens: Vec<_> = app
        .terminal_runtimes
        .iter()
        .map(|(id, _)| (id.clone(), app.terminal_runtimes.hc_token(id).unwrap()))
        .collect();
    let source = app
        .terminal_factory
        .source_state()
        .ok_or_else(|| invalid("no reconnectable source"))?;
    let source_generation = source.generation;
    let old_connection = source.client.ok_or_else(disconnected)?;
    old_connection.disconnect();
    old_connection.closed().await;
    // Change the source screen with no App attachment; recovery must fetch it, not replay input.
    let (rows, cols) = app.terminal_runtimes.get(&id).unwrap().current_size();
    let (_, mut writer) = probe
        .open(
            "terminal.attach",
            json!({"terminal":root_address,"rows":rows,"cols":cols}),
        )
        .await?;
    writer
        .input(Bytes::from_static(b"printf 'HC_OUTAGE_OUTPUT\\n'\r"), false)
        .await?;
    timeout(Duration::from_secs(10), async {
        let mut output = Vec::new();
        loop {
            let frame = writer.recv().await?;
            if frame.kind == DATA {
                output.extend_from_slice(&frame.payload);
                writer.consumed(frame.payload.len()).await?;
                if output
                    .windows(b"HC_OUTAGE_OUTPUT".len())
                    .any(|s| s == b"HC_OUTAGE_OUTPUT")
                {
                    return Ok::<_, io::Error>(());
                }
            }
        }
    })
    .await
    .map_err(|_| invalid("scratch source output was not generated"))??;
    drop(writer);
    timeout(Duration::from_secs(15),async{
        loop {
            app.render_dirty.take();
            app.run_hc_recovery(std::time::Instant::now());
            let source_ready=app.terminal_factory.source_state().is_some_and(|s|s.generation>source_generation&&s.client.is_some());
            let replaced=old_tokens.iter().all(|(id,token)|app.terminal_runtimes.hc_token(id).is_some_and(|current|!Arc::ptr_eq(&current,token))&&app.terminal_runtimes.get(id).and_then(|r|r.hc_snapshot()).is_some_and(|s|s.connected&&s.attached));
            if source_ready&&replaced&&app.terminal_runtimes.get(&id).unwrap().visible_text().contains("HC_OUTAGE_OUTPUT"){break;}
            let due=app.hc_recovery.deadline().unwrap_or_else(||std::time::Instant::now()+Duration::from_secs(60));
            tokio::select! {
                event=app.event_rx.recv()=>{app.handle_internal_event_with_render_impact(event.ok_or_else(disconnected)?);},
                _=app.render_notify.notified()=>{},
                _=tokio::time::sleep_until(due.into())=>{},
            }
        }
        Ok::<_,io::Error>(())
    }).await.map_err(|_|invalid("source outage did not recover existing App bindings"))??;
    assert_eq!(app_bindings(&app), bindings);
    assert_eq!(
        live_programs(&probe).await?,
        before_reconnect,
        "transport recovery launched, replaced or killed a program"
    );
    assert_eq!(
        app.public_pane_id(ws, root).as_deref(),
        Some(public_root.as_str())
    );
    assert_eq!(
        app.state.terminals[&id].state,
        crate::detect::AgentState::Blocked
    );
    let snapshot = crate::persist::capture(
        &app.state.workspaces,
        &app.state.terminals,
        &app.terminal_runtimes,
        app.state.active,
        app.state.selected,
    );
    let json = serde_json::to_vec(&snapshot).map_err(io::Error::other)?;
    let snapshot: crate::persist::SessionSnapshot =
        serde_json::from_slice(&json).map_err(io::Error::other)?;
    assert_eq!(snapshot.version, 4);
    let programs = live_programs(&probe).await?;
    assert_eq!(programs.len(), 3);
    drop(app);
    assert_eq!(
        live_programs(&probe).await?,
        programs,
        "App drop terminated an hc program"
    );
    let mut restored = App::try_new_with_terminal_factory(
        &config,
        AppPolicy::TEST,
        None,
        tokio::sync::mpsc::unbounded_channel().1,
        crate::api::EventHub::default(),
        TerminalFactory::connect(target.command(&["connect"])),
    )?;
    let (workspaces, terminals, runtimes) = crate::persist::restore_with_factory(
        &snapshot,
        None,
        24,
        80,
        1024,
        "/must-not-relaunch",
        crate::config::ShellModeConfig::NonLogin,
        true,
        restored.event_tx.clone(),
        restored.render_notify.clone(),
        restored.render_dirty.clone(),
        &restored.terminal_factory,
    );
    restored.state.workspaces = workspaces;
    restored.state.terminals = terminals;
    restored.state.active = snapshot.active;
    restored.state.selected = snapshot.selected;
    restored.terminal_runtimes.extend(runtimes);
    assert!(!restored.ensure_default_workspace());
    restored.state.assert_invariants_for_test();
    assert_eq!(app_bindings(&restored), bindings);
    assert_eq!(restored.terminal_runtimes.len(), 3);
    assert_eq!(
        live_programs(&probe).await?,
        programs,
        "restore replaced or relaunched a program"
    );
    for mismatch in [true, false] {
        let mut invalid: crate::persist::SessionSnapshot =
            serde_json::from_slice(&json).map_err(io::Error::other)?;
        for workspace in &mut invalid.workspaces {
            for tab in &mut workspace.tabs {
                for pane in tab.panes.values_mut() {
                    let binding = pane.hc_binding.as_mut().unwrap();
                    if mismatch {
                        binding["core_id"] = json!("another-permanent-core");
                    } else {
                        binding["address"] = json!("t999999");
                    }
                }
            }
        }
        let (workspaces, terminals, runtimes) = crate::persist::restore_with_factory(
            &invalid,
            None,
            24,
            80,
            1024,
            "/must-not-relaunch",
            crate::config::ShellModeConfig::NonLogin,
            true,
            restored.event_tx.clone(),
            restored.render_notify.clone(),
            restored.render_dirty.clone(),
            &restored.terminal_factory,
        );
        assert_eq!(workspaces[0].tabs.len(), 2);
        assert_eq!(terminals.len(), 3);
        assert!(runtimes.is_empty());
        assert!(terminals
            .values()
            .all(|terminal| terminal.hc_binding.is_some()
                && terminal.restore_error.is_some()
                && terminal.pending_agent_resume_plan.is_none()));
        assert_eq!(
            live_programs(&probe).await?,
            programs,
            "failed restore launched or killed a program"
        );
    }
    app_request(&mut restored, "pane.close", json!({"pane_id":public_root}))?;
    let mut remaining = programs;
    remaining.remove(&root_address);
    assert_eq!(
        live_programs(&probe).await?,
        remaining,
        "pane close affected another hc terminal"
    );
    app_request(
        &mut restored,
        "workspace.close",
        json!({"workspace_id":public_workspace}),
    )?;
    assert!(restored.state.workspaces.is_empty());
    assert!(
        !restored.ensure_default_workspace(),
        "closing the last hc pane automatically respawned a shell"
    );
    assert!(live_programs(&probe).await?.is_empty());

    // Real bash exits must remove presentation without turning completion into kill/run.
    restored.state.default_shell = "/bin/bash".into();
    let ws = restored.create_workspace_with_options("/var/tmp".into(), true)?;
    let root = restored.state.workspaces[ws].tabs[0].root_pane;
    let root_id = restored.state.workspaces[ws]
        .terminal_id(root)
        .unwrap()
        .clone();
    let public = restored.public_pane_id(ws, root).unwrap();
    app_request(
        &mut restored,
        "pane.split",
        json!({"target_pane_id":public,"direction":"right","focus":false}),
    )?;
    let split_id = restored.state.workspaces[ws].tabs[0]
        .panes
        .values()
        .find(|pane| pane.attached_terminal_id != root_id)
        .unwrap()
        .attached_terminal_id
        .clone();
    restored.spawn_popup_argv_command(
        &["/bin/bash".into(), "--noprofile".into(), "--norc".into()],
        Some("/var/tmp".into()),
        Vec::new(),
        Default::default(),
    )?;
    let popup_id = restored
        .state
        .popup_pane
        .as_ref()
        .unwrap()
        .terminal_id
        .clone();
    for (index, id) in [popup_id, split_id, root_id].into_iter().enumerate() {
        let binding = restored.state.terminals[&id].hc_binding.clone().unwrap();
        let mut expected = live_programs(&probe).await?;
        expected.remove(binding["address"].as_str().unwrap());
        restored
            .terminal_runtimes
            .get(&id)
            .unwrap()
            .try_send_bytes(Bytes::from_static(if index == 1 {
                b"exit 7\r"
            } else {
                b"exit\r"
            }))
            .map_err(io::Error::other)?;
        timeout(Duration::from_secs(10), async {
            while restored.state.terminals.contains_key(&id) {
                let event = restored.event_rx.recv().await.ok_or_else(disconnected)?;
                restored.handle_internal_event_with_render_impact(event);
            }
            Ok::<_, io::Error>(())
        })
        .await
        .map_err(|_| invalid("bash exited but its pane remained"))??;
        assert!(restored.terminal_runtimes.get(&id).is_none());
        assert_eq!(
            live_programs(&probe).await?,
            expected,
            "bash exit changed another source program"
        );
        restored.state.assert_invariants_for_test();
    }
    assert!(restored.state.workspaces.is_empty());
    assert!(restored.state.popup_pane.is_none());
    assert!(
        !restored.ensure_default_workspace(),
        "bash exit respawned the last shell"
    );
    println!(
        "App: create/split/tab/popup, attach-only transport recovery/PID + public-ID preservation, JSON binding roundtrip, detach/restore, mismatch/missing, exact close/no respawn, bash exit removes popup/split/last workspace verified"
    );
    Ok(())
}

#[tokio::test]
async fn hc_smoke_retains_both_roots_when_presentation_cleanup_is_unconfirmed() {
    let target = Target {
        host: None,
        executable: "/must-not-execute".into(),
        scratch: "/must-not-remove-core".into(),
        presentation_pending: Some(PathBuf::from("/must-not-remove-presenter")),
    };
    let error = target
        .cleanup()
        .await
        .expect_err("unconfirmed presenter allowed core removal");
    assert!(error.to_string().contains("/must-not-remove-core"));
    assert!(error.to_string().contains("/must-not-remove-presenter"));
}

#[tokio::test]
async fn hc_smoke_cleanup_checks_nested_source_sockets_before_removing_evidence() {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
    let root = PathBuf::from("/var/tmp").join(format!(
        "hs-{}-{:x}",
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
    let nested = root.join("herdr-core");
    std::fs::create_dir(&nested).unwrap();
    let listener = crate::ipc::bind_private_local_listener(&nested.join("holder.sock")).unwrap();
    let stopper = root.join("stop");
    std::fs::write(&stopper, "#!/bin/sh\nprintf '%s\\n' '{\"stopped\":true}'\n").unwrap();
    std::fs::set_permissions(&stopper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let target = Target {
        host: None,
        executable: stopper.to_str().unwrap().into(),
        scratch: root.to_str().unwrap().into(),
        presentation_pending: None,
    };
    let error = target
        .cleanup()
        .await
        .expect_err("reachable nested socket was removed");
    assert!(error.to_string().contains("still reachable"));
    assert!(root.exists());
    drop(listener);
    target.cleanup().await.unwrap();
    assert!(!root.exists());
}

#[test]
#[ignore = "needs installed upstream hc; creates only an isolated scratch core"]
fn hc_multiview_live_qualification() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("multiview executor");
    let target = runtime.block_on(target()).expect("scratch target");
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(multiview::exercise(&target))
    }));
    let cleanup = runtime.block_on(target.cleanup());
    assert!(
        cleanup.is_ok(),
        "cleanup failed for {}: {cleanup:?}",
        target.scratch
    );
    match outcome {
        Ok(result) => assert!(result.is_ok(), "hc multiview failed: {result:?}"),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

#[test]
#[ignore = "needs installed upstream hc; creates only an isolated scratch core"]
fn hc_native_live_smoke() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("smoke executor");
    let mut target = runtime.block_on(target()).expect("scratch target");
    // A failed assertion must still stop the presenter before its private core.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(async {
            exercise(&target).await?;
            exercise_runtime(&target).await?;
            exercise_app(&target).await
        })?;
        saved::run(&mut target, &runtime)
    }));
    let cleanup = runtime.block_on(target.cleanup());
    assert!(
        cleanup.is_ok(),
        "cleanup failed for {}: {cleanup:?}",
        target.scratch
    );
    match outcome {
        Ok(result) => assert!(result.is_ok(), "native hc smoke failed: {result:?}"),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}
