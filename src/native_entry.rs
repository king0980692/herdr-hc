//! `herdr-native`: the Native frontend as its own app, beside an installed
//! herdr. Its own app directory keeps its servers, sockets and saved layout
//! apart from herdr's, so the one binary runs from any terminal.
//!
//! The policy below is compiled into every build so its tests always run; the
//! `native-entry` feature decides whether the binary applies it.

use std::path::Path;

pub(crate) const ENABLED: bool = cfg!(feature = "native-entry");

pub(crate) fn app_dir_name(debug: bool) -> &'static str {
    if debug {
        "herdr-native-dev"
    } else {
        "herdr-native"
    }
}

/// What a pane of a Herdr server sets for the programs in it.
const PANE_ENV: &[&str] = &[
    crate::api::SOCKET_PATH_ENV_VAR,
    crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR,
    crate::session::SESSION_ENV_VAR,
    crate::config::CONFIG_PATH_ENV_VAR,
    crate::remote::REMOTE_KEYBINDINGS_ENV_VAR,
    crate::HERDR_ENV_VAR,
    crate::integration::HERDR_PANE_ID_ENV_VAR,
    crate::integration::HERDR_TAB_ID_ENV_VAR,
    crate::integration::HERDR_WORKSPACE_ID_ENV_VAR,
];

/// Whether a socket path belongs to this app (it lies in one of its
/// directories) rather than to another Herdr app.
fn is_own(socket: &Path, app_dir: &str) -> bool {
    socket
        .components()
        .any(|component| component.as_os_str() == app_dir)
}

/// Started from a pane of another Herdr app (herdr itself, say): what that
/// pane set points at the other app's server, so it is dropped. A pane of
/// this app keeps it, and running inside one is still nesting.
pub(crate) fn drop_foreign_pane_env() {
    let app_dir = app_dir_name(cfg!(debug_assertions));
    let foreign = std::env::var_os(crate::api::SOCKET_PATH_ENV_VAR)
        .is_some_and(|socket| !is_own(Path::new(&socket), app_dir));
    if foreign {
        for key in PANE_ENV {
            // Edition 2021: no other thread exists yet at this point of `main`.
            std::env::remove_var(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_sockets_in_this_apps_directories_are_its_own() {
        let app = app_dir_name(false);
        assert!(is_own(
            Path::new("/home/u/.config/herdr-native/herdr.sock"),
            app
        ));
        assert!(is_own(
            Path::new("/home/u/.config/herdr-native/sessions/alt/herdr.sock"),
            app
        ));
        assert!(!is_own(Path::new("/home/u/.config/herdr/herdr.sock"), app));
        assert!(!is_own(
            Path::new("/home/u/.config/herdr-native-old/herdr.sock"),
            app
        ));
        assert_ne!(app, app_dir_name(true));
    }
}
