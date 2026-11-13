//! `herdr-hc`: this build as a Native view of the machine's Local hc core only.
//!
//! The policy below is compiled into every build so its tests always run; the
//! `hc-entry` feature decides whether the binary applies it.

use std::{collections::HashSet, io, sync::OnceLock};

pub(crate) mod app_layout;
pub(crate) mod app_sync;

pub(crate) const ENABLED: bool = cfg!(feature = "hc-entry");

/// The program a launching client runs inside, handed to the server it starts
/// so first open does not project the client's own terminal into itself.
const HOST_PID_ENV: &str = "HERDR_HC_ENTRY_HOST_PID";

/// Inherited selectors that would point this process at another Herdr session,
/// a saved or shared source, or a different hc core. Other `HERDR_*` stay.
const INHERITED_SELECTORS: &[&str] = &[
    crate::session::SESSION_ENV_VAR,
    crate::api::SOCKET_PATH_ENV_VAR,
    crate::api::STATE_DIR_ENV_VAR,
    crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR,
    crate::config::CONFIG_PATH_ENV_VAR,
    crate::remote::REMOTE_KEYBINDINGS_ENV_VAR,
    crate::hc::source::PROFILE_ENV,
    crate::hc::shared::CORE_ENV,
    crate::hc::shared::REGISTRY_ENV,
    "HC_SOCKET",
    "HC_SESSION",
    "HC_TERMINAL_ID",
    "HC_TERMINAL_SOCKET",
    HOST_PID_ENV,
];

static HOST_PID: OnceLock<Option<u32>> = OnceLock::new();

pub(crate) fn app_dir_name(debug: bool) -> &'static str {
    if debug {
        "herdr-hc-dev"
    } else {
        "herdr-hc"
    }
}

/// Options and commands that reach another session, source or release channel.
pub(crate) fn check_args(args: &[String]) -> Result<(), String> {
    let refuse = |what: &str| {
        Err(format!(
            "herdr-hc shows only this machine's Local hc core; `{what}` is not available"
        ))
    };
    let mut command = None;
    for (index, arg) in args.iter().enumerate().skip(1) {
        if !arg.starts_with('-') {
            command = Some(index);
            break;
        }
        let name = arg.split_once('=').map_or(arg.as_str(), |(name, _)| name);
        if matches!(
            name,
            "--remote" | "--remote-keybindings" | "--session" | "--machine" | "--handoff"
        ) {
            return refuse(name);
        }
    }
    let Some(index) = command else {
        return Ok(());
    };
    match args[index].as_str() {
        name @ ("update" | "channel" | "machine") => refuse(name),
        "session" if args.get(index + 1).map(String::as_str) == Some("attach") => {
            refuse("session attach")
        }
        _ => Ok(()),
    }
}

/// Runs before anything reads the environment: refuses other sessions and
/// sources, keeps the host handed over by a launching client, then removes
/// every inherited selector.
pub(crate) fn prepare_process(args: &[String]) -> Result<(), String> {
    check_args(args)?;
    let _ = HOST_PID.set(
        std::env::var(HOST_PID_ENV)
            .ok()
            .and_then(|pid| pid.parse().ok()),
    );
    for key in INHERITED_SELECTORS {
        // Edition 2021: no other thread exists yet at this point of `main`.
        std::env::remove_var(key);
    }
    Ok(())
}

/// Program PIDs that first open must leave out of the layout.
pub(crate) fn excluded_program_pids() -> HashSet<u32> {
    HOST_PID.get().copied().flatten().into_iter().collect()
}

/// A client's ancestry, starting with itself.
fn ancestry(start: u32, parent: impl Fn(u32) -> Option<u32>) -> Vec<u32> {
    let mut chain = vec![start];
    while chain.len() < 128 {
        match parent(*chain.last().unwrap_or(&start)) {
            Some(next) if next > 1 && !chain.contains(&next) => chain.push(next),
            _ => break,
        }
    }
    chain
}

/// The Local program whose terminal the client runs in, if any.
fn hosting_program<'a>(ancestry: &[u32], live: &'a [(String, u32)]) -> Option<&'a (String, u32)> {
    ancestry
        .iter()
        .find_map(|pid| live.iter().find(|(_, program)| program == pid))
}

/// Live programs on the Local core, as `(address, pid)`.
fn local_live_programs() -> io::Result<Vec<(String, u32)>> {
    std::thread::spawn(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(async {
                let client =
                    crate::hc::Client::connect(crate::hc::shared::registry_command()).await?;
                let reply = client
                    .call("terminal.list", serde_json::json!({"all": false}))
                    .await;
                client.disconnect();
                let reply = reply?;
                let terminals = reply["terminals"].as_array().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "hc terminal list is incomplete")
                })?;
                Ok(terminals
                    .iter()
                    .filter_map(|terminal| {
                        Some((
                            terminal["address"].as_str()?.to_owned(),
                            u32::try_from(terminal["pid"].as_u64()?).ok()?,
                        ))
                    })
                    .collect())
            })
    })
    .join()
    .map_err(|_| io::Error::other("hc terminal discovery panicked"))?
}

/// PIDs of the programs a running herdr-hc server already presents.
fn presented_program_pids() -> io::Result<Vec<u32>> {
    let client = crate::api::client::ApiClient::local();
    let request = |method: &str, params: serde_json::Value| -> io::Result<serde_json::Value> {
        let request: crate::api::schema::Request = serde_json::from_value(
            serde_json::json!({"id":"hc-entry-host","method":method,"params":params}),
        )
        .map_err(io::Error::other)?;
        client.request_value(&request).map_err(io::Error::other)
    };
    let panes = request("pane.list", serde_json::json!({}))?;
    let panes = panes["result"]["panes"]
        .as_array()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "pane list is incomplete"))?;
    let mut pids = Vec::new();
    for pane in panes {
        let Some(pane_id) = pane["pane_id"].as_str() else {
            continue;
        };
        let info = request(
            "pane.process_info",
            serde_json::json!({ "pane_id": pane_id }),
        )?;
        if let Some(pid) = info["result"]["process_info"]["shell_pid"]
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok())
        {
            pids.push(pid);
        }
    }
    Ok(pids)
}

/// Before the TUI starts: a client must never present the terminal it runs in.
/// Outside any Local hc program nothing changes. Inside one, a server this
/// launch starts leaves that program out of first open; a running server that
/// already presents it (or has no layout yet) is refused.
pub(crate) fn prepare_tui_launch(server_running: bool) -> Result<(), String> {
    let live = local_live_programs()
        .map_err(|error| format!("herdr-hc cannot read the Local hc core: {error}"))?;
    let chain = ancestry(std::process::id(), crate::platform::parent_pid);
    let Some((address, pid)) = hosting_program(&chain, &live) else {
        return Ok(());
    };
    if !server_running {
        // Inherited only by the server this launch starts.
        std::env::set_var(HOST_PID_ENV, pid.to_string());
        return Ok(());
    }
    let presented = presented_program_pids()
        .map_err(|error| format!("herdr-hc cannot read its running layout: {error}"))?;
    if presented.is_empty() || presented.contains(pid) {
        return Err(format!(
            "this terminal is hc program {address}, and herdr-hc would show it inside itself; open herdr-hc from another terminal"
        ));
    }
    Ok(())
}

/// herdr.app shows a core terminal only when it carries `herdr.space=<id>`;
/// `herdr.tab=<id>` picks its tab, and ids it does not know become a new space
/// or tab (herdr-app `defaults/AGENTS.md`, "Every terminal is a core terminal").
/// A terminal herdr-native runs for itself (a remote plugin bridge): never
/// shown as a program, nor placed in a layout.
pub(crate) const HIDDEN_LABEL: &str = "herdr-native.hidden";

/// Whether a terminal record carries [`HIDDEN_LABEL`].
pub(crate) fn is_hidden(facts: &serde_json::Map<String, serde_json::Value>) -> bool {
    facts
        .get("labels")
        .and_then(|labels| labels.get(HIDDEN_LABEL))
        .is_some_and(|value| value == "1")
}

pub(crate) const SPACE_LABEL: &str = "herdr.space";
pub(crate) const TAB_LABEL: &str = "herdr.tab";
/// herdr.app starts its own terminals at `herdr.seen=0` (herdr-app `docs/labels.md`).
const SEEN_LABEL: &str = "herdr.seen";

/// Which herdr.app space and tab each herdr-hc workspace and tab maps to,
/// learned from the labels of the programs it shows. Keyed by herdr-hc's
/// public workspace and tab ids.
#[derive(Debug, Default)]
pub(crate) struct Placements {
    tabs: std::collections::HashMap<(String, String), (String, String)>,
    spaces: std::collections::HashMap<String, String>,
}

impl Placements {
    /// Records where a program in this herdr-hc tab lives in herdr.app.
    pub(crate) fn learn(&mut self, workspace: &str, tab: &str, space: &str, space_tab: &str) {
        if space.is_empty() || space_tab.is_empty() {
            return;
        }
        self.spaces.insert(workspace.to_owned(), space.to_owned());
        self.tabs.insert(
            (workspace.to_owned(), tab.to_owned()),
            (space.to_owned(), space_tab.to_owned()),
        );
    }

    /// herdr.app's space and tab for this herdr tab, when known.
    pub(crate) fn tab(&self, workspace: &str, tab: &str) -> Option<(String, String)> {
        self.tabs
            .get(&(workspace.to_owned(), tab.to_owned()))
            .cloned()
    }

    /// herdr.app's space for this herdr workspace, when known.
    pub(crate) fn space(&self, workspace: &str) -> Option<String> {
        self.spaces.get(workspace).cloned()
    }

    /// Labels for a new program in this herdr-hc tab: its tab's herdr.app
    /// placement, else its workspace's space with a new tab, else a new space.
    pub(crate) fn labels_for_new_program(
        &mut self,
        workspace: &str,
        tab: &str,
        mut new_id: impl FnMut(&str) -> String,
    ) -> serde_json::Map<String, serde_json::Value> {
        let key = (workspace.to_owned(), tab.to_owned());
        let (space, space_tab) = match self.tabs.get(&key) {
            Some(placement) => placement.clone(),
            None => {
                let space = self
                    .spaces
                    .get(workspace)
                    .cloned()
                    .unwrap_or_else(|| new_id("sp"));
                (space, new_id("tb"))
            }
        };
        self.learn(workspace, tab, &space, &space_tab);
        let mut labels = serde_json::Map::new();
        labels.insert(SPACE_LABEL.into(), space.into());
        labels.insert(TAB_LABEL.into(), space_tab.into());
        labels.insert(SEEN_LABEL.into(), "0".into());
        labels
    }
}

/// A fresh placement id, unique across processes and calls.
pub(crate) fn new_placement_id(prefix: &str) -> String {
    use std::hash::{BuildHasher, Hasher};
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u32(std::process::id());
    hasher.write_u64(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos()),
    );
    format!("{prefix}{:016x}", hasher.finish())
}

/// A core this entry can share programs with: terminal-generated replies must
/// not take another viewer's size.
pub(crate) fn core_supported(hello: &crate::hc::Hello) -> bool {
    hello.features.contains("automatic_input")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn hc_entry_refuses_other_sessions_sources_and_release_channels() {
        for refused in [
            &["herdr-hc", "--remote", "host"][..],
            &["herdr-hc", "--remote=host"],
            &["herdr-hc", "--remote-keybindings", "local"],
            &["herdr-hc", "--session", "work"],
            &["herdr-hc", "--session=work", "pane", "list"],
            &["herdr-hc", "--machine", "gx10", "pane", "list"],
            &["herdr-hc", "update"],
            &["herdr-hc", "update", "--handoff"],
            &["herdr-hc", "channel", "set", "preview"],
            &["herdr-hc", "machine", "add", "host"],
            &["herdr-hc", "session", "attach", "work"],
        ] {
            assert!(check_args(&args(refused)).is_err(), "{refused:?}");
        }
        for allowed in [
            &["herdr-hc"][..],
            &["herdr-hc", "--version"],
            &["herdr-hc", "server"],
            &["herdr-hc", "server", "stop"],
            &["herdr-hc", "pane", "list"],
            &["herdr-hc", "session", "list"],
            // Text after the command is data, not an option.
            &[
                "herdr-hc",
                "pane",
                "run",
                "w1:p1",
                "herdr update --remote x",
            ],
            &["herdr-hc", "agent", "prompt", "w1:p1", "--session"],
        ] {
            assert!(check_args(&args(allowed)).is_ok(), "{allowed:?}");
        }
    }

    #[test]
    fn hc_entry_uses_its_own_app_directory() {
        assert_eq!(app_dir_name(false), "herdr-hc");
        assert_eq!(app_dir_name(true), "herdr-hc-dev");
    }

    #[test]
    fn hc_entry_finds_the_program_hosting_the_client_by_ancestry() {
        let parents = |pid| match pid {
            900 => Some(800),
            800 => Some(700),
            700 => Some(1),
            _ => None,
        };
        assert_eq!(ancestry(900, parents), vec![900, 800, 700]);
        assert_eq!(ancestry(5, |_| Some(5)), vec![5]);

        let live = vec![("t3".to_owned(), 800), ("t4".to_owned(), 42)];
        assert_eq!(
            hosting_program(&[900, 800, 700], &live),
            Some(&("t3".to_owned(), 800))
        );
        // The nearest ancestor wins when terminals are nested.
        let nested = vec![("outer".to_owned(), 700), ("inner".to_owned(), 800)];
        assert_eq!(
            hosting_program(&[900, 800, 700], &nested).map(|(address, _)| address.as_str()),
            Some("inner")
        );
        assert_eq!(hosting_program(&[900, 600], &live), None);
    }

    #[test]
    fn hc_entry_places_new_programs_where_herdr_app_shows_their_tab() {
        let mut ids = 0;
        let mut new_id = |prefix: &str| {
            ids += 1;
            format!("{prefix}{ids}")
        };
        let mut placements = Placements::default();
        // A program herdr.app started, now shown in herdr-hc's w1:t1.
        placements.learn("w1", "w1:t1", "spApp", "tbApp");

        // A split in that tab joins the same herdr.app tab.
        let split = placements.labels_for_new_program("w1", "w1:t1", &mut new_id);
        assert_eq!(split[SPACE_LABEL], "spApp");
        assert_eq!(split[TAB_LABEL], "tbApp");
        assert_eq!(split[SEEN_LABEL], "0");

        // A new tab in that workspace gets a new tab in the same space, and
        // keeps it for later splits.
        let tab = placements.labels_for_new_program("w1", "w1:t2", &mut new_id);
        assert_eq!(tab[SPACE_LABEL], "spApp");
        assert_eq!(tab[TAB_LABEL], "tb1");
        let again = placements.labels_for_new_program("w1", "w1:t2", &mut new_id);
        assert_eq!(again[TAB_LABEL], "tb1");

        // A workspace herdr.app has never seen gets its own space.
        let fresh = placements.labels_for_new_program("w2", "w2:t1", &mut new_id);
        assert_eq!(fresh[SPACE_LABEL], "sp2");
        assert_eq!(fresh[TAB_LABEL], "tb3");

        // Empty labels (a terminal herdr.app parked) teach nothing.
        placements.learn("w3", "w3:t1", "", "");
        let parked = placements.labels_for_new_program("w3", "w3:t1", &mut new_id);
        assert_eq!(parked[SPACE_LABEL], "sp4");

        assert_ne!(new_placement_id("sp"), new_placement_id("sp"));
    }

    #[test]
    fn hc_entry_requires_a_core_that_separates_automatic_replies() {
        let hello = |features: &[&str]| crate::hc::Hello {
            generation: 1,
            framed: true,
            core: crate::hc::CoreInfo {
                id: "core".into(),
                name: "local".into(),
            },
            version: "0.1.19".into(),
            methods: Default::default(),
            features: features
                .iter()
                .map(|feature| (*feature).to_owned())
                .collect(),
        };
        assert!(core_supported(&hello(&["automatic_input"])));
        assert!(!core_supported(&hello(&[])));
    }
}
