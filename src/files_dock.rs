//! The files dock's program: herdr-sidebar, built into herdr-native and run
//! as `herdr-native sidebar …` (feature `files-dock`).

/// The command that runs the built-in sidebar.
pub(crate) const COMMAND: &str = "sidebar";

/// The same sidebar as a plugin, one per tab.
const REPLACED_PLUGIN: &str = "herdr-sidebar";

/// What the dock keeps of the plugin's state, the first time it runs.
const SEEDED_FILES: &[&str] = &["state.json", "roots.json", "tree.json", "scm.json"];

/// The first time the dock runs, it starts from where the plugin left off:
/// remembered roots, expanded folders, Source Control drawers and drafts.
fn seed_from_plugin(dock_dir: &std::path::Path, plugin_dir: &std::path::Path) {
    if dock_dir.join("state.json").exists() || !plugin_dir.is_dir() {
        return;
    }
    if let Err(error) = std::fs::create_dir_all(dock_dir) {
        tracing::warn!(%error, "the files dock's state directory could not be made");
        return;
    }
    for file in SEEDED_FILES {
        let from = plugin_dir.join(file);
        if from.is_file() {
            if let Err(error) = std::fs::copy(&from, dock_dir.join(file)) {
                tracing::warn!(%error, file, "the plugin's sidebar state could not be copied");
            }
        }
    }
}

/// Runs the sidebar with the arguments after `sidebar`. It runs itself again
/// (preview viewer, editor, transfer servers) through this same command.
pub(crate) fn run(raw_args: &[String]) -> std::io::Result<()> {
    if let Ok(program) = std::env::current_exe() {
        herdr_sidebar::host::set_invocation(program, vec![COMMAND.to_owned()]);
    }
    herdr_sidebar::cli::main_with_args(raw_args.iter().skip(2).cloned().collect())
}

/// The files dock's session terminal: this program's sidebar, docked.
pub(crate) fn session_terminal_spec() -> Option<crate::app::session_terminal::SessionTerminalSpec> {
    let program = std::env::current_exe().ok()?;
    let state_dir = crate::config::config_dir().join("files");
    seed_from_plugin(
        &state_dir,
        &crate::plugin_paths::plugin_state_dir(REPLACED_PLUGIN),
    );
    let env = vec![(
        "HERDR_PLUGIN_STATE_DIR".to_owned(),
        state_dir.display().to_string(),
    )];
    Some(crate::app::session_terminal::SessionTerminalSpec {
        role: "files".to_owned(),
        argv: vec![
            program.display().to_string(),
            COMMAND.to_owned(),
            "--dock".to_owned(),
        ],
        env,
        settings: Some(crate::app::session_terminal::ComponentSettingsSpec {
            title: "Files".to_owned(),
            argv: vec![program.display().to_string(), COMMAND.to_owned()],
        }),
        replaces_plugin: Some(REPLACED_PLUGIN.to_owned()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dock_starts_from_the_plugins_state_only_once() {
        let base = std::env::temp_dir().join(format!(
            "herdr-files-dock-seed-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let (plugin, dock) = (base.join("plugin"), base.join("dock"));
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(plugin.join("roots.json"), "{\"a\":1}").unwrap();
        std::fs::write(plugin.join("state.json"), "{}").unwrap();
        std::fs::write(plugin.join("other.lock"), "").unwrap();
        seed_from_plugin(&dock, &plugin);
        assert_eq!(
            std::fs::read_to_string(dock.join("roots.json")).unwrap(),
            "{\"a\":1}"
        );
        assert!(!dock.join("other.lock").exists());
        // The dock's own state wins from then on.
        std::fs::write(plugin.join("roots.json"), "{\"b\":2}").unwrap();
        seed_from_plugin(&dock, &plugin);
        assert_eq!(
            std::fs::read_to_string(dock.join("roots.json")).unwrap(),
            "{\"a\":1}"
        );
        let _ = std::fs::remove_dir_all(base);
    }
}
