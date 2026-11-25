//! Settings of the session's components (the files dock), shown in the
//! client's settings page and changed through the server, which keeps them.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ComponentSettingKind {
    Toggle,
    Choice {
        options: Vec<String>,
    },
    Text,
    /// A kind this client does not know: shown, not changed.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ComponentSetting {
    pub key: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    pub kind: ComponentSettingKind,
    pub value: serde_json::Value,
    #[serde(default = "enabled")]
    pub enabled: bool,
}

fn enabled() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SettingsComponent {
    /// What the component is (`files`).
    pub id: String,
    pub title: String,
    pub settings: Vec<ComponentSetting>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ComponentSettingSetParams {
    pub component: String,
    pub key: String,
    pub value: serde_json::Value,
}
