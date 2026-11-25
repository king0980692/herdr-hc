//! The files dock's program: herdr-sidebar, built into herdr-native and run
//! as `herdr-native sidebar …` (feature `files-dock`).

/// The command that runs the built-in sidebar.
pub(crate) const COMMAND: &str = "sidebar";

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
    let env = vec![(
        "HERDR_PLUGIN_STATE_DIR".to_owned(),
        crate::config::config_dir()
            .join("files")
            .display()
            .to_string(),
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
    })
}
