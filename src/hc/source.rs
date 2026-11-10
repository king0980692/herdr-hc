//! Rebuildable control transports. Locators route connections, never identify programs.
use std::{ffi::OsString, io, path::PathBuf};
use tokio::process::Command;

pub(crate) const PROFILE_ENV: &str = "HERDR_HC_SOURCE_PROFILE";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceLocator {
    pub profile_id: String,
    pub target: String,
    pub session: String,
    pub hc_path: Option<String>,
}

#[derive(Clone)]
pub(crate) enum Source {
    Command(CommandSpec),
    Saved(String),
}

#[derive(Clone)]
pub(crate) struct CommandSpec {
    program: OsString,
    args: Vec<OsString>,
    env: Vec<(OsString, Option<OsString>)>,
    cwd: Option<PathBuf>,
}

impl Source {
    pub(crate) fn command(command: Command) -> Self {
        let command = command.as_std();
        Self::Command(CommandSpec {
            program: command.get_program().to_owned(),
            args: command.get_args().map(ToOwned::to_owned).collect(),
            env: command
                .get_envs()
                .map(|(k, v)| (k.to_owned(), v.map(ToOwned::to_owned)))
                .collect(),
            cwd: command.get_current_dir().map(ToOwned::to_owned),
        })
    }

    pub(crate) fn locator(&self) -> io::Result<Option<SourceLocator>> {
        let Self::Saved(id) = self else {
            return Ok(None);
        };
        let profiles =
            crate::client::endpoint::EndpointCatalog::load_profiles().map_err(io::Error::other)?;
        let profile = profiles
            .into_iter()
            .find(|p| p.id.as_str() == id && p.enabled)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "hc source profile is missing or disabled",
                )
            })?;
        Ok(Some(SourceLocator {
            profile_id: id.clone(),
            target: profile.target,
            session: profile.session,
            hc_path: profile.hc_path,
        }))
    }

    pub(crate) fn connect_command(
        &self,
        locator: Option<&SourceLocator>,
    ) -> io::Result<(Command, Option<crate::remote::RemoteSsh>)> {
        match self {
            Self::Command(spec) => {
                let mut command = Command::new(&spec.program);
                command.args(&spec.args);
                for (key, value) in &spec.env {
                    match value {
                        Some(value) => {
                            command.env(key, value);
                        }
                        None => {
                            command.env_remove(key);
                        }
                    }
                }
                if let Some(cwd) = &spec.cwd {
                    command.current_dir(cwd);
                }
                Ok((command, None))
            }
            Self::Saved(_) => {
                let locator =
                    locator.ok_or_else(|| io::Error::other("hc source profile is unavailable"))?;
                ssh_command(
                    &locator.target,
                    &locator.session,
                    locator.hc_path.as_deref(),
                )
                .map(|(command, ssh)| (command, Some(ssh)))
            }
        }
    }
}

// Keep the policy owner alive through the hc hello: OpenSSH reads its temporary config after spawn.
pub(crate) fn validate_hc_path(path: &str) -> Result<(), String> {
    if !path.starts_with('/') || path.len() > 4096 || path.chars().any(char::is_control) {
        return Err("hc path must be an absolute source-local path of at most 4096 bytes with no control characters".into());
    }
    Ok(())
}

pub(crate) fn ssh_command(
    target: &str,
    session: &str,
    hc_path: Option<&str>,
) -> io::Result<(Command, crate::remote::RemoteSsh)> {
    crate::remote::validate_remote_target(target)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    crate::session::validate_name(session)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    if let Some(path) = hc_path {
        validate_hc_path(path).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    }
    let executable = crate::remote::shell_quote(hc_path.unwrap_or("hc"));
    let ssh = crate::remote::RemoteSsh::new_noninteractive(target.to_owned());
    let mut command = Command::from(ssh.command());
    // The remote shell expands its own HOME/PATH. Never forward client environment or run Herdr.
    command.arg(format!("export PATH=\"$HOME/.local/bin:$HOME/.cargo/bin:/usr/local/bin:$PATH\"; unset HC_SOCKET HC_TERMINAL_ID HERDR_HC_SOURCE_PROFILE; export HC_SESSION={}; exec {executable} connect",crate::remote::shell_quote(session)));
    Ok((command, ssh))
}

struct CancelSshRepair(std::sync::Arc<std::sync::atomic::AtomicBool>);
impl Drop for CancelSshRepair {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

pub(crate) async fn repair_ssh(policy: crate::remote::RemoteSsh) -> io::Result<bool> {
    let _cancel = CancelSshRepair(policy.cancelled.clone());
    tokio::task::spawn_blocking(move || policy.prepare_shared_transport())
        .await
        .map_err(io::Error::other)?
}

pub(crate) fn retry_delay(attempt: u32) -> std::time::Duration {
    std::time::Duration::from_millis(500u64 << attempt.min(6))
        .min(std::time::Duration::from_secs(30))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn saved_source_connects_only_hc_without_client_environment_or_interactive_ssh() {
        let source = Source::Saved("0123456789abcdef0123456789abcdef".into());
        let (command, policy) = source
            .connect_command(Some(&SourceLocator {
                profile_id: "0123456789abcdef0123456789abcdef".into(),
                target: "gx10".into(),
                session: "agent-work".into(),
                hc_path: None,
            }))
            .unwrap();
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"BatchMode=yes".into()));
        assert!(args.contains(&"ServerAliveCountMax=4".into()));
        assert!(args.contains(&"StrictHostKeyChecking=yes".into()));
        assert!(args.contains(&"NumberOfPasswordPrompts=0".into()));
        assert!(policy.is_some());
        let config = args
            .windows(2)
            .find(|pair| pair[0] == "-F")
            .map(|pair| std::path::PathBuf::from(&pair[1]));
        if let Some(path) = &config {
            assert!(path.is_file());
        }
        let script = args.last().unwrap();
        assert!(script.contains("exec hc connect"));
        assert!(script.contains("HC_SESSION=agent-work"));
        assert!(ssh_command("gx10", "agent work; echo bad", None).is_err());
        assert!(!script.contains("herdr"));
        assert!(!script.contains("/Users/"));
        assert!(ssh_command("-oProxyCommand=bad", "default", None).is_err());
        drop(command);
        drop(policy);
        if let Some(path) = config {
            assert!(!path.exists());
        }
    }
    #[test]
    fn custom_hc_path_is_one_quoted_executable_and_never_falls_back() {
        let path = "/opt/hc with 'quotes'; echo unsafe";
        let (command, _policy) = ssh_command("source", "agents", Some(path)).unwrap();
        let script = command
            .as_std()
            .get_args()
            .last()
            .unwrap()
            .to_str()
            .unwrap();
        assert!(script.ends_with(&format!(
            "exec {} connect",
            crate::remote::shell_quote(path)
        )));
        assert!(!script.contains("||"));
        assert!(!script.contains("exec hc connect"));
        for invalid in ["", "hc", "../hc", "~/bin/hc", "/hc\nrun", "/hc\0run"] {
            assert!(
                ssh_command("source", "agents", Some(invalid)).is_err(),
                "{invalid:?}"
            );
        }
        assert!(validate_hc_path(&format!("/{}", "a".repeat(4096))).is_err());
    }

    #[test]
    fn hc_ssh_repair_cancellation_is_scoped_to_its_operation() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };
        let stopped = Arc::new(AtomicBool::new(false));
        let unrelated = Arc::new(AtomicBool::new(false));
        {
            let _cancel = CancelSshRepair(stopped.clone());
            assert!(!stopped.load(Ordering::Acquire));
        }
        assert!(stopped.load(Ordering::Acquire));
        assert!(!unrelated.load(Ordering::Acquire));
    }
    #[test]
    fn retry_is_bounded_and_does_not_overflow() {
        assert_eq!(retry_delay(0).as_millis(), 500);
        assert_eq!(retry_delay(1).as_millis(), 1000);
        assert_eq!(retry_delay(u32::MAX).as_secs(), 30);
    }
}
