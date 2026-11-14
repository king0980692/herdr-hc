//! App-owned runtime creation. A restored binding is never a launch request.
use super::{
    connection::{CoreHost, SourceState},
    TerminalRuntime,
};
use crate::{
    events::AppEvent,
    hc::{source::Source, state::TerminalBinding, Client},
    layout::PaneId,
    pane::{AgentDetection, PaneLaunchEnv, PaneShellConfig},
    render_signal::RenderSignal,
    terminal_theme::{HostAppearance, TerminalTheme},
};
use serde_json::{json, Value};
use std::{io, path::PathBuf, sync::Arc};

#[derive(Clone, Copy)]
pub(crate) enum TerminalCommand<'a> {
    Shell(PaneShellConfig<'a>),
    Argv(&'a [String], AgentDetection),
    Line(&'a str, AgentDetection),
}
pub(crate) enum TerminalFactory {
    Hc(Box<CoreHost>),
    Unavailable(String),
    #[cfg(test)]
    Legacy,
}
impl TerminalFactory {
    #[cfg(not(test))]
    pub(crate) fn local() -> io::Result<Self> {
        if let Some(core_id) = std::env::var_os(crate::hc::shared::CORE_ENV) {
            if std::env::var_os(crate::hc::source::PROFILE_ENV).is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "conflicting hc source selectors",
                ));
            }
            let registry = std::env::var(crate::hc::shared::REGISTRY_ENV).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "shared hc registry core is required",
                )
            })?;
            return CoreHost::connect(Source::Shared(crate::hc::shared::SharedSource::new(
                core_id.to_string_lossy().into_owned(),
                registry,
            )))
            .map(|host| Self::Hc(Box::new(host)));
        }
        if let Some(id) = std::env::var_os(crate::hc::source::PROFILE_ENV) {
            return Self::connect_saved(id.to_string_lossy().into_owned(), CoreHost::connect);
        }
        Ok(Self::connect(crate::hc::shared::registry_command()))
    }
    fn connect_saved(
        id: String,
        connect: impl FnOnce(Source) -> io::Result<CoreHost>,
    ) -> io::Result<Self> {
        // Executor failure must stop startup, not fall back to client-local filesystem policy.
        connect(Source::Saved(id)).map(|host| Self::Hc(Box::new(host)))
    }
    #[cfg(test)]
    pub(crate) fn test_hc(client: Client) -> Self {
        Self::Hc(Box::new(CoreHost::test(client)))
    }
    #[cfg(test)]
    pub(crate) fn test_source(
        client: Option<Client>,
    ) -> (Self, tokio::sync::watch::Sender<SourceState>) {
        let (host, send) = CoreHost::test_source(client, false);
        (Self::Hc(Box::new(host)), send)
    }
    #[cfg(test)]
    pub(crate) fn test_saved_source() -> Self {
        Self::Hc(Box::new(CoreHost::test_source(None, true).0))
    }

    // Production startup is compiled out of tests; there only the Unix live checks use it.
    #[cfg_attr(all(test, not(unix)), allow(dead_code))]
    pub(crate) fn connect(command: tokio::process::Command) -> Self {
        match CoreHost::connect(Source::command(command)) {
            Ok(host) => Self::Hc(Box::new(host)),
            Err(e) => Self::Unavailable(e.to_string()),
        }
    }
    /// The core's live programs, and (herdr-hc builds) herdr.app's layout of
    /// them when it has one. The layout is advisory: unreadable means `None`.
    pub(crate) fn initial_terminals(
        &self,
    ) -> io::Result<(String, Vec<crate::hc::state::Terminal>, Option<Value>)> {
        let Self::Hc(host) = self else {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "hc terminal discovery is unavailable",
            ));
        };
        let source = host.state();
        let client = host.client()?;
        let reply = host.block_on(client.call("terminal.list", json!({"all":false})))?;
        let layout = if crate::hc_entry::ENABLED
            && !host.saved
            && client.hello.methods.contains("doc.get")
        {
            match host.block_on(client.call(
                "doc.get",
                json!({ "name": crate::hc_entry::app_layout::DOCUMENT }),
            )) {
                Ok(reply) => Some(reply["value"].clone()).filter(|value| !value.is_null()),
                Err(error) => {
                    tracing::warn!(%error, "herdr.app layout unavailable; programs open as tabs");
                    None
                }
            }
        } else {
            None
        };
        let current = host.state();
        if current.generation != source.generation
            || current.retired
            || current
                .client
                .as_ref()
                .is_none_or(|peer| peer.hello.core.id != client.hello.core.id)
        {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "hc source changed during terminal discovery; layout was not replaced",
            ));
        }
        let records = serde_json::from_value(reply.get("terminals").cloned().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "hc terminal discovery has no complete list",
            )
        })?)
        .map_err(io::Error::other)?;
        Ok((client.hello.core.id.clone(), records, layout))
    }

    pub(crate) fn source_state(&self) -> Option<SourceState> {
        match self {
            Self::Hc(host) => Some(host.state()),
            _ => None,
        }
    }
    pub(crate) fn shared_machine_graph(&self) -> io::Result<crate::hc::machines::MachineGraph> {
        let Self::Hc(host) = self else {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "hc source is unavailable",
            ));
        };
        if host.saved {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "shared machine inventory is available on Local",
            ));
        }
        let client = host.client()?;
        let graph = host
            .machines
            .lock()
            .map_err(|_| io::Error::other("hc machine inventory lock failed"))?
            .clone()
            .map_err(|error| io::Error::new(io::ErrorKind::NotConnected, error))?;
        if graph.core.id != client.hello.core.id {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "hc machine inventory belongs to a previous local core",
            ));
        }
        Ok(graph)
    }
    #[cfg(test)]
    pub(crate) fn test_machine_graph(
        &self,
        graph: Result<crate::hc::machines::MachineGraph, String>,
    ) {
        let Self::Hc(host) = self else {
            panic!("expected hc test factory")
        };
        *host.machines.lock().unwrap() = graph;
    }
    pub(crate) fn watch_source(&self, events: tokio::sync::mpsc::Sender<AppEvent>) {
        if let Self::Hc(host) = self {
            host.watch(events);
        }
    }
    /// Reports live programs that appear on a discovering source's core
    /// after this viewer opened (not in herdr-hc builds: herdr.app's layout
    /// places those).
    // Started only by production startup; fake-core unit tests never subscribe.
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn watch_programs(&self, events: tokio::sync::mpsc::Sender<AppEvent>) {
        if let Self::Hc(host) = self {
            if !crate::hc_entry::ENABLED && self.discovers_existing_programs() {
                host.watch_programs(events);
            }
        }
    }
    /// Whether this viewer keeps its layout in step with herdr.app's shared
    /// document on its core (Native builds; herdr-hc writes splits only).
    pub(crate) fn shares_herdr_app_layout(&self) -> bool {
        let Self::Hc(host) = self else {
            return false;
        };
        !crate::hc_entry::ENABLED
            && self.discovers_existing_programs()
            && host.client().is_ok_and(|client| {
                ["doc.get", "doc.set", "terminal.list"]
                    .iter()
                    .all(|method| client.hello.methods.contains(*method))
            })
    }

    /// One round off the app thread: read the core's live programs and the
    /// document, apply this viewer's changes since `base` (on a first sync,
    /// `None`: the document's layout plus what only this viewer shows), and
    /// write it back. Reports `HcLayoutSynced`.
    pub(crate) fn sync_herdr_app_layout(
        &self,
        generation: u64,
        base: Option<Vec<crate::hc_entry::app_layout::ShownSpace>>,
        native: Vec<crate::hc_entry::app_layout::ShownSpace>,
        events: tokio::sync::mpsc::Sender<AppEvent>,
    ) -> bool {
        let Self::Hc(host) = self else {
            return false;
        };
        let Ok(client) = host.client() else {
            return false;
        };
        host.executor.spawn(async move {
            let outcome = sync_layout_once(&client, base, native)
                .await
                .map_err(|error| error.to_string());
            let _ = events
                .send(AppEvent::HcLayoutSynced {
                    generation,
                    outcome,
                })
                .await;
        });
        true
    }

    /// Delivers `event` to the app after `delay`, off its thread.
    pub(crate) fn send_later(
        &self,
        event: AppEvent,
        delay: std::time::Duration,
        events: tokio::sync::mpsc::Sender<AppEvent>,
    ) {
        if let Self::Hc(host) = self {
            host.executor.spawn(async move {
                tokio::time::sleep(delay).await;
                let _ = events.send(event).await;
            });
        }
    }

    pub(crate) fn discovers_existing_programs(&self) -> bool {
        matches!(self, Self::Hc(host) if !host.saved || host.shared)
    }
    /// Records a split herdr-hc made in herdr.app's shared layout, so herdr.app
    /// shows `new` beside `source` along the same axis instead of adopting it
    /// beside its tab's last pane. Runs off the caller's thread; a second pass
    /// a moment later moves the pane back if herdr.app adopted it meanwhile.
    pub(crate) fn place_split_in_herdr_app(
        &self,
        source: String,
        new: String,
        row: bool,
        ratio: f64,
    ) {
        let Self::Hc(host) = self else {
            return;
        };
        if !crate::hc_entry::ENABLED || host.saved {
            return;
        }
        let Ok(client) = host.client() else {
            return;
        };
        if !["doc.get", "doc.set"]
            .iter()
            .all(|method| client.hello.methods.contains(*method))
        {
            return;
        }
        host.executor.spawn(async move {
            for pass in 0..2 {
                if pass > 0 {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
                if let Err(error) = place_split_once(&client, &source, &new, row, ratio).await {
                    tracing::warn!(%error, %source, %new, "herdr.app layout was not updated for a split");
                    return;
                }
            }
        });
    }

    /// Remembers where a shown program lives in herdr.app, so new programs in
    /// the same herdr workspace and tab land beside it there.
    pub(crate) fn learn_placement(&self, workspace: &str, tab: &str, labels: &Value) {
        let Self::Hc(host) = self else {
            return;
        };
        if !crate::hc_entry::ENABLED || host.saved {
            return;
        }
        let label = |key| labels.get(key).and_then(Value::as_str).unwrap_or_default();
        if let Ok(mut placements) = host.placements.lock() {
            placements.learn(
                workspace,
                tab,
                label(crate::hc_entry::SPACE_LABEL),
                label(crate::hc_entry::TAB_LABEL),
            );
        }
    }

    /// Records which herdr.app space and tab a herdr workspace and tab are,
    /// for the labels of programs started there.
    pub(crate) fn place(&self, workspace: &str, tab: &str, space: &str, space_tab: &str) {
        if let Self::Hc(host) = self {
            if let Ok(mut placements) = host.placements.lock() {
                placements.learn(workspace, tab, space, space_tab);
            }
        }
    }

    /// herdr.app's space and tab already given to a herdr tab (a program
    /// started there carries them), if any.
    pub(crate) fn placement(&self, workspace: &str, tab: &str) -> (Option<String>, Option<String>) {
        let Self::Hc(host) = self else {
            return (None, None);
        };
        let Ok(placements) = host.placements.lock() else {
            return (None, None);
        };
        match placements.tab(workspace, tab) {
            Some((space, space_tab)) => (Some(space), Some(space_tab)),
            None => (placements.space(workspace), None),
        }
    }

    /// Programs start on a shared remote machine (a `hcs-` presenter), not
    /// on the machine this process runs on.
    /// On a shared remote machine's presenter: serve the plugin bridge there.
    #[cfg(all(unix, not(test)))]
    pub(crate) fn watch_bridge(&self, api_socket: std::path::PathBuf) {
        if let Self::Hc(host) = self {
            if self.runs_on_shared_machine() {
                host.watch_bridge(api_socket);
            }
        }
    }

    pub(crate) fn runs_on_shared_machine(&self) -> bool {
        matches!(self, Self::Hc(host) if host.saved && host.shared)
    }

    pub(crate) fn is_saved_source(&self) -> bool {
        matches!(self,Self::Hc(host) if host.saved)
    }
    pub(crate) fn attach_source(
        &self,
        binding: &TerminalBinding,
    ) -> io::Result<(u64, Client, tokio::runtime::Handle)> {
        binding.validate()?;
        let host = match self {
            Self::Hc(host) => host,
            Self::Unavailable(error) => {
                return Err(io::Error::new(io::ErrorKind::NotConnected, error.clone()))
            }
            #[cfg(test)]
            Self::Legacy => {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "hc source unavailable",
                ))
            }
        };
        let source = host.state();
        let client = source.client.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                source
                    .error
                    .unwrap_or_else(|| "hc source unavailable".into()),
            )
        })?;
        if client.hello.core.id != binding.core_id {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "hc core identity changed; binding was retained",
            ));
        }
        Ok((source.generation, client, host.executor.clone()))
    }
    // Creation shares existing geometry, launch policy and presentation hooks.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn(
        &self,
        pane: PaneId,
        rows: u16,
        cols: u16,
        cwd: PathBuf,
        scrollback: usize,
        theme: TerminalTheme,
        appearance: Option<HostAppearance>,
        command: TerminalCommand<'_>,
        env: &PaneLaunchEnv,
        events: tokio::sync::mpsc::Sender<AppEvent>,
        notify: Arc<tokio::sync::Notify>,
        dirty: Arc<RenderSignal>,
    ) -> io::Result<TerminalRuntime> {
        match self {
            Self::Unavailable(e) => Err(io::Error::new(io::ErrorKind::NotConnected, e.clone())),
            #[cfg(test)]
            Self::Legacy => match command {
                TerminalCommand::Shell(shell) => TerminalRuntime::spawn(
                    pane, rows, cols, cwd, scrollback, theme, appearance, shell, env, events,
                    notify, dirty,
                ),
                TerminalCommand::Argv(argv, detection) => TerminalRuntime::spawn_argv_command(
                    pane, rows, cols, cwd, argv, env, detection, scrollback, theme, appearance,
                    events, notify, dirty,
                ),
                TerminalCommand::Line(line, detection) => TerminalRuntime::spawn_shell_command(
                    pane, rows, cols, cwd, line, env, detection, scrollback, theme, appearance,
                    events, notify, dirty,
                ),
            },
            Self::Hc(host) => {
                let client = host.client()?;
                if client.hello.core.id.trim().is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "hc has no permanent core identity",
                    ));
                }
                let source_argv = if host.saved {
                    source_shell_argv(command)
                } else {
                    None
                };
                let command = source_argv
                    .as_ref()
                    .map(|argv| TerminalCommand::Argv(argv, AgentDetection::Disabled))
                    .unwrap_or(command);
                // herdr-hc, and Native sharing herdr.app's layout, label programs
                // with their herdr.app space and tab, as herdr.app does its own.
                let placed = crate::hc_entry::ENABLED || self.shares_herdr_app_layout();
                let labels = env
                    .workspace_and_tab()
                    // A shared machine's programs are placed in its own document.
                    .filter(|_| placed && (!host.saved || host.shared))
                    .and_then(|(workspace, tab)| {
                        let mut placements = host.placements.lock().ok()?;
                        Some(placements.labels_for_new_program(
                            workspace,
                            tab,
                            crate::hc_entry::new_placement_id,
                        ))
                    })
                    .unwrap_or_default();
                let (argv, env) =
                    crate::pane::hc_launch_command(command, env, self.is_saved_source())?;
                let cwd = cwd.to_str().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "hc cwd must be UTF-8")
                })?;
                host.block_on(async {
                    // Never retry run: timeout/disconnect can mean the program already exists.
                    let result=client.call("terminal.run",json!({"argv":argv,"env":env,"cwd":(!cwd.is_empty()).then_some(cwd),"size":{"rows":rows.max(crate::pane::MIN_PANE_ROWS),"cols":cols.max(crate::pane::MIN_PANE_COLS)},"no_parent":true,"no_notify":true,"labels":labels,"keep":false})).await?;
                    let address=result.get("address").and_then(Value::as_str).ok_or_else(||io::Error::new(io::ErrorKind::InvalidData,"hc run returned no address; outcome unknown"))?;
                    let binding=TerminalBinding{core_id:client.hello.core.id.clone(),address:address.to_owned()};
                    binding.validate().map_err(|e|io::Error::new(e.kind(),format!("hc run returned an invalid binding; outcome unknown: {e}")))?;
                    match TerminalRuntime::attach_hc(pane,rows,cols,client.clone(),address,0,scrollback,theme,appearance,events,notify,dirty,true).await {
                        Ok(runtime)=>Ok(runtime),
                        Err(e)=>match kill(&client,address).await {Ok(())=>Err(e),Err(cleanup)=>Err(io::Error::new(e.kind(),format!("{e}; cleanup of {}/{address} failed: {cleanup}",binding.core_id)))},
                    }
                })
            }
        }
    }
    // Restore never applies shell/history/resume launch policy.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn restore(
        &self,
        binding: &TerminalBinding,
        pane: PaneId,
        rows: u16,
        cols: u16,
        scrollback: usize,
        theme: TerminalTheme,
        appearance: Option<HostAppearance>,
        events: tokio::sync::mpsc::Sender<AppEvent>,
        notify: Arc<tokio::sync::Notify>,
        dirty: Arc<RenderSignal>,
    ) -> io::Result<TerminalRuntime> {
        let (_, client, _) = self.attach_source(binding)?;
        let Self::Hc(host) = self else {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "hc source unavailable",
            ));
        };
        host.block_on(TerminalRuntime::attach_hc(
            pane,
            rows,
            cols,
            client,
            &binding.address,
            crate::pane::REATTACH_HISTORY_LINES,
            scrollback,
            theme,
            appearance,
            events,
            notify,
            dirty,
            true,
        ))
    }
    pub(crate) fn terminate(
        &self,
        binding: &TerminalBinding,
        runtime: Option<&TerminalRuntime>,
    ) -> io::Result<()> {
        let (_, client, _) = self.attach_source(binding)?;
        if runtime.is_some_and(|r| r.hc_snapshot().is_none_or(|s| s.binding != *binding)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "hc runtime and saved binding disagree",
            ));
        }
        let Self::Hc(host) = self else {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "hc source unavailable",
            ));
        };
        host.block_on(async {
            let list = client.call("terminal.list", json!({"all":true})).await?;
            let records = list
                .get("terminals")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid hc terminal list")
                })?;
            let mut addresses = std::collections::HashSet::new();
            for record in records {
                if record["address"]
                    .as_str()
                    .is_none_or(|a| a.is_empty() || !addresses.insert(a))
                    || record["state"].as_str().is_none_or(str::is_empty)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid hc terminal list; absence was not confirmed",
                    ));
                }
            }
            let record = records
                .iter()
                .find(|r| r["address"].as_str() == Some(&binding.address));
            match record.and_then(|r| r["state"].as_str()) {
                None if record.is_none() => Ok(()),
                Some("finished") => Ok(()),
                Some("live") => kill(&client, &binding.address).await,
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unknown hc lifecycle; binding was retained",
                )),
            }
        })
    }
    pub(crate) fn is_hc(&self) -> bool {
        match self {
            #[cfg(test)]
            Self::Legacy => false,
            _ => true,
        }
    }
}
fn source_shell_argv(command: TerminalCommand<'_>) -> Option<Vec<String>> {
    match command {
        TerminalCommand::Shell(shell) if shell.default_shell.trim().is_empty() => Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            match shell.mode {
                crate::config::ShellModeConfig::NonLogin => "exec \"${SHELL:-/bin/sh}\"",
                _ => "exec \"${SHELL:-/bin/sh}\" -l",
            }
            .into(),
        ]),
        TerminalCommand::Line(line, _) => Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            "exec \"${SHELL:-/bin/sh}\" -c \"$1\"".into(),
            "hc-command".into(),
            line.into(),
        ]),
        _ => None,
    }
}

async fn kill(client: &Client, address: &str) -> io::Result<()> {
    // Use the captured connection: a concurrent source change must not kill another core's tN.
    let result = client
        .call("terminal.kill", json!({"terminal":address}))
        .await?;
    if result["address"].as_str() != Some(address) || result["state"] != "finished" {
        return Err(io::Error::other(format!(
            "hc did not confirm termination of {}/{address}; binding must be retained",
            client.hello.core.id
        )));
    }
    Ok(())
}

/// One read-modify-write of herdr.app's layout, retried on a version conflict.
async fn sync_layout_once(
    client: &crate::hc::Client,
    base: Option<Vec<crate::hc_entry::app_layout::ShownSpace>>,
    native: Vec<crate::hc_entry::app_layout::ShownSpace>,
) -> io::Result<(
    Value,
    Vec<String>,
    Vec<crate::hc_entry::app_layout::ShownSpace>,
)> {
    use crate::hc_entry::{
        app_layout::{shown_spaces, DOCUMENT},
        app_sync::{held_terminals, push_changes, restricted, union},
    };
    for _ in 0..4 {
        // The document first: a program in it was started before the list
        // below is read, so missing from that list means it ended.
        let current = client.call("doc.get", json!({ "name": DOCUMENT })).await?;
        let version = current["version"].as_u64().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "herdr.app layout has no version",
            )
        })?;
        let mut document = current["value"].clone();
        let decodable = document.is_null()
            || (document.is_object()
                && document
                    .get("spaces")
                    .is_none_or(serde_json::Value::is_array));
        if !decodable {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "herdr.app layout is not one this build can read; it was left as it is",
            ));
        }
        let list = client.call("terminal.list", json!({"all": false})).await?;
        let live: std::collections::HashSet<String> = list["terminals"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|terminal| terminal["state"] == "live")
            .filter_map(|terminal| terminal["address"].as_str())
            .filter(|address| !address.contains('/'))
            .map(str::to_owned)
            .collect();
        let mut left = live.clone();
        let shown = shown_spaces(&document, |address| left.remove(address));
        // Panes of programs that ended stay open here; they are not layout.
        let native = restricted(&native, |address| live.contains(address));
        let (base, sent) = match &base {
            // The base keeps programs that ended since: closing one here is a change.
            Some(base) => (base.clone(), native),
            None => {
                let sent = union(&shown, &native, &held_terminals(&document));
                (shown, sent)
            }
        };
        let held_before = held_terminals(&document);
        if push_changes(
            &mut document,
            &base,
            &sent,
            |address| live.contains(address),
            crate::hc_entry::new_placement_id,
        ) {
            match client
                .call(
                    "doc.set",
                    json!({ "name": DOCUMENT, "version": version, "value": document }),
                )
                .await
            {
                Ok(_) => {
                    // herdr.app keeps the programs it shows (it adopts with
                    // `keep: true`); one this viewer placed is never adopted by it.
                    if client.hello.methods.contains("terminal.keep") {
                        for address in held_terminals(&document)
                            .difference(&held_before)
                            .filter(|address| live.contains(*address))
                        {
                            client
                                .call(
                                    "terminal.keep",
                                    json!({ "terminal": address, "keep": true }),
                                )
                                .await?;
                        }
                    }
                }
                Err(error) if error.to_string().contains("\"conflict\"") => continue,
                Err(error) => return Err(error),
            }
        }
        let mut live: Vec<String> = live.into_iter().collect();
        live.sort();
        return Ok((document, live, sent));
    }
    Err(io::Error::other("herdr.app layout kept changing"))
}

async fn place_split_once(
    client: &crate::hc::Client,
    source: &str,
    new: &str,
    row: bool,
    ratio: f64,
) -> io::Result<()> {
    use crate::hc_entry::app_layout::{place_split, SplitPlacement, DOCUMENT};
    for _ in 0..4 {
        let current = client.call("doc.get", json!({ "name": DOCUMENT })).await?;
        // No herdr.app has written a layout on this core: nothing to keep in step.
        if current["value"].is_null() {
            return Ok(());
        }
        let version = current["version"].as_u64().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "herdr.app layout has no version",
            )
        })?;
        let mut document = current["value"].clone();
        if place_split(
            &mut document,
            source,
            new,
            row,
            ratio,
            crate::hc_entry::new_placement_id,
        ) != SplitPlacement::Placed
        {
            return Ok(());
        }
        match client
            .call(
                "doc.set",
                json!({ "name": DOCUMENT, "version": version, "value": document }),
            )
            .await
        {
            // herdr.app keeps the programs it shows (it adopts with
            // `keep: true`); one herdr-hc writes in is never adopted by it.
            Ok(_) if client.hello.methods.contains("terminal.keep") => {
                client
                    .call("terminal.keep", json!({ "terminal": new, "keep": true }))
                    .await?;
                return Ok(());
            }
            Ok(_) => return Ok(()),
            Err(error) if error.to_string().contains("\"conflict\"") => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::other("herdr.app layout kept changing"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hc_saved_executor_startup_failure_never_becomes_a_local_factory() {
        let result = TerminalFactory::connect_saved("saved-profile".into(), |source| {
            assert!(matches!(source, Source::Saved(_)));
            Err(io::Error::other("executor startup failed"))
        });
        assert_eq!(result.err().unwrap().to_string(), "executor startup failed");
    }
    #[test]
    fn hc_source_shell_uses_source_environment_and_keeps_commands_as_arguments() {
        let shell = PaneShellConfig::new("", crate::config::ShellModeConfig::NonLogin);
        let argv = source_shell_argv(TerminalCommand::Shell(shell)).unwrap();
        assert_eq!(argv, ["/bin/sh", "-c", "exec \"${SHELL:-/bin/sh}\""]);
        assert!(
            source_shell_argv(TerminalCommand::Shell(PaneShellConfig::new(
                "/explicit/source/shell",
                crate::config::ShellModeConfig::Login
            )))
            .is_none()
        );
        let line = "printf 'quoted $words'";
        let argv =
            source_shell_argv(TerminalCommand::Line(line, AgentDetection::Disabled)).unwrap();
        assert_eq!(argv.last().map(String::as_str), Some(line));
        assert!(argv[2].contains("$1"));
    }
}
