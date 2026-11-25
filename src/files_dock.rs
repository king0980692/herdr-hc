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
