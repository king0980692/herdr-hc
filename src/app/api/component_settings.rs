//! `component_settings.*`: the settings of the session's components (the
//! files dock), kept where the server runs. The component's own program
//! lists and changes them (`--settings-list`, `--settings-set`), in the
//! environment it runs in.

use crate::api::schema::{ComponentSettingSetParams, ResponseResult, SettingsComponent};
use crate::app::App;

use super::responses::{encode_error, encode_success};

/// How long a component may take to answer.
const ANSWER_WITHIN: std::time::Duration = std::time::Duration::from_secs(5);

/// A component with settings: its id, how to ask it, and its environment.
type SettingsComponentRun = (
    String,
    crate::app::session_terminal::ComponentSettingsSpec,
    Vec<(String, String)>,
);

impl App {
    fn settings_component(&self) -> Option<SettingsComponentRun> {
        let spec = self.session_terminal_spec.as_ref()?;
        Some((spec.role.clone(), spec.settings.clone()?, spec.env.clone()))
    }

    fn component_settings(&self) -> Result<Vec<SettingsComponent>, String> {
        let Some((id, settings, env)) = self.settings_component() else {
            return Ok(Vec::new());
        };
        let output = self.run_component(&settings, &env, &["--settings-list"])?;
        let settings = serde_json::from_slice(&output)
            .map_err(|error| format!("{id} settings are not understood: {error}"))?;
        Ok(vec![SettingsComponent {
            id,
            title: settings_title(&self.session_terminal_spec),
            settings,
        }])
    }

    pub(super) fn handle_component_settings_list(&mut self, id: String) -> String {
        match self.component_settings() {
            Ok(components) => encode_success(id, ResponseResult::ComponentSettings { components }),
            Err(message) => encode_error(id, "component_settings_unavailable", message),
        }
    }

    pub(super) fn handle_component_settings_set(
        &mut self,
        id: String,
        params: ComponentSettingSetParams,
    ) -> String {
        let Some((component, settings, env)) = self
            .settings_component()
            .filter(|(component, _, _)| *component == params.component)
        else {
            return encode_error(
                id,
                "component_not_found",
                format!("no component {}", params.component),
            );
        };
        let value = params.value.to_string();
        if let Err(message) =
            self.run_component(&settings, &env, &["--settings-set", &params.key, &value])
        {
            return encode_error(id, "invalid_setting", message);
        }
        tracing::info!(component, key = %params.key, "component setting changed");
        self.handle_component_settings_list(id)
    }
}

impl App {
    /// The component's command with `args`: here, or on the source's
    /// machine where it runs.
    fn run_component(
        &self,
        settings: &crate::app::session_terminal::ComponentSettingsSpec,
        env: &[(String, String)],
        args: &[&str],
    ) -> Result<Vec<u8>, String> {
        if !settings.on_source {
            return run(&settings.argv, env, args);
        }
        let mut argv = settings.argv.clone();
        argv.extend(args.iter().map(|arg| (*arg).to_owned()));
        self.terminal_factory
            .exec_on_source_now(argv, ANSWER_WITHIN.as_secs())
            .map_err(|message| component_error(&message))
    }
}

fn settings_title(spec: &Option<crate::app::session_terminal::SessionTerminalSpec>) -> String {
    spec.as_ref()
        .and_then(|spec| spec.settings.as_ref())
        .map(|settings| settings.title.clone())
        .unwrap_or_default()
}

/// Runs `argv args…` with `env`; its output, or its error message.
fn run(argv: &[String], env: &[(String, String)], args: &[&str]) -> Result<Vec<u8>, String> {
    let (program, prefix) = argv
        .split_first()
        .ok_or_else(|| "the component has no command".to_owned())?;
    let mut child = std::process::Command::new(program)
        .args(prefix)
        .args(args)
        .envs(env.iter().map(|(key, value)| (key, value)))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("the component could not run: {error}"))?;
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < ANSWER_WITHIN => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("the component did not answer in time".into());
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(component_error(&String::from_utf8_lossy(&output.stderr)))
    }
}

/// What a component said on failing, without Rust's error dressing.
fn component_error(stderr: &str) -> String {
    stderr
        .trim()
        .trim_start_matches("Error: ")
        .trim_start_matches("Custom { kind: Other, error: ")
        .trim_end_matches(" }")
        .trim_matches('"')
        .to_owned()
}

#[cfg(all(test, unix))]
mod tests {
    use crate::api::schema::{Method, Request, ResponseResult, SuccessResponse};

    fn app_with_component(dir: &std::path::Path) -> crate::app::App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = crate::app::App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        // A component that lists one toggle and records what is set.
        let script = format!(
            r#"case "$1" in
  --settings-list) printf '[{{"key":"follow","label":"Follow","kind":{{"type":"toggle"}},"value":%s,"enabled":true}}]' "$(cat '{dir}/value' 2>/dev/null || echo true)" ;;
  --settings-set) [ "$2" = follow ] || {{ echo "Error: no setting $2" >&2; exit 1; }}; printf %s "$3" > '{dir}/value' ;;
esac"#,
            dir = dir.display()
        );
        app.session_terminal_spec = Some(crate::app::session_terminal::SessionTerminalSpec {
            role: "files".into(),
            argv: vec!["true".into()],
            env: Vec::new(),
            settings: Some(crate::app::session_terminal::ComponentSettingsSpec {
                title: "Files".into(),
                argv: vec!["sh".into(), "-c".into(), script, "component".into()],
                on_source: false,
            }),
            replaces_plugin: None,
            needs_bridge: false,
        });
        app
    }

    fn call(app: &mut crate::app::App, method: Method) -> serde_json::Value {
        serde_json::from_str(&app.handle_api_request(Request {
            id: "settings".into(),
            method,
        }))
        .unwrap()
    }

    #[test]
    fn component_settings_are_listed_and_changed_by_the_component_itself() {
        let dir = std::env::temp_dir().join(format!(
            "herdr-component-settings-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = app_with_component(&dir);

        let listed = call(
            &mut app,
            Method::ComponentSettingsList(crate::api::schema::EmptyParams::default()),
        );
        let response: SuccessResponse = serde_json::from_value(listed.clone()).unwrap();
        let ResponseResult::ComponentSettings { components } = response.result else {
            panic!("{listed}");
        };
        assert_eq!(components[0].id, "files");
        assert_eq!(components[0].title, "Files");
        assert_eq!(
            components[0].settings[0].value,
            serde_json::Value::Bool(true)
        );

        // A change answers with the updated list.
        let set = call(
            &mut app,
            Method::ComponentSettingsSet(crate::api::schema::ComponentSettingSetParams {
                component: "files".into(),
                key: "follow".into(),
                value: serde_json::Value::Bool(false),
            }),
        );
        assert_eq!(
            set["result"]["components"][0]["settings"][0]["value"],
            serde_json::Value::Bool(false),
            "{set}"
        );
        // The component's own refusal comes back as the error.
        let refused = call(
            &mut app,
            Method::ComponentSettingsSet(crate::api::schema::ComponentSettingSetParams {
                component: "files".into(),
                key: "missing".into(),
                value: serde_json::Value::Bool(true),
            }),
        );
        assert_eq!(refused["error"]["code"], "invalid_setting", "{refused}");
        assert!(
            refused["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("no setting missing")),
            "{refused}"
        );
        // No such component.
        let unknown = call(
            &mut app,
            Method::ComponentSettingsSet(crate::api::schema::ComponentSettingSetParams {
                component: "other".into(),
                key: "follow".into(),
                value: serde_json::Value::Bool(true),
            }),
        );
        assert_eq!(unknown["error"]["code"], "component_not_found");
        let _ = std::fs::remove_dir_all(dir);
    }
}
