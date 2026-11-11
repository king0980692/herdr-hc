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
    Hc(CoreHost),
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
            .map(Self::Hc);
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
        connect(Source::Saved(id)).map(Self::Hc)
    }
    #[cfg(test)]
    pub(crate) fn test_hc(client: Client) -> Self {
        Self::Hc(CoreHost::test(client))
    }
    #[cfg(test)]
    pub(crate) fn test_source(
        client: Option<Client>,
    ) -> (Self, tokio::sync::watch::Sender<SourceState>) {
        let (host, send) = CoreHost::test_source(client, false);
        (Self::Hc(host), send)
    }
    #[cfg(test)]
    pub(crate) fn test_saved_source() -> Self {
        Self::Hc(CoreHost::test_source(None, true).0)
    }

    pub(crate) fn connect(command: tokio::process::Command) -> Self {
        match CoreHost::connect(Source::command(command)) {
            Ok(host) => Self::Hc(host),
            Err(e) => Self::Unavailable(e.to_string()),
        }
    }
    pub(crate) fn initial_terminals(
        &self,
    ) -> io::Result<(String, Vec<crate::hc::state::Terminal>)> {
        let Self::Hc(host) = self else {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "hc terminal discovery is unavailable",
            ));
        };
        let source = host.state();
        let client = host.client()?;
        let reply = host.block_on(client.call("terminal.list", json!({"all":false})))?;
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
        Ok((client.hello.core.id.clone(), records))
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
    pub(crate) fn discovers_existing_programs(&self) -> bool {
        matches!(self, Self::Hc(host) if !host.saved || host.shared)
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
                let (argv, env) =
                    crate::pane::hc_launch_command(command, env, self.is_saved_source())?;
                let cwd = cwd.to_str().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "hc cwd must be UTF-8")
                })?;
                host.block_on(async {
                    // Never retry run: timeout/disconnect can mean the program already exists.
                    let result=client.call("terminal.run",json!({"argv":argv,"env":env,"cwd":(!cwd.is_empty()).then_some(cwd),"size":{"rows":rows.max(crate::pane::MIN_PANE_ROWS),"cols":cols.max(crate::pane::MIN_PANE_COLS)},"no_parent":true,"no_notify":true,"keep":false})).await?;
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
            0,
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
