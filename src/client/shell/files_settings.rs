//! The settings page's Files section: this client's dock (shown, width),
//! then the endpoint's component settings (the dock's sidebar), which the
//! server keeps where it runs. Enter toggles a toggle, cycles a choice, or
//! edits a line of text in place.

use crate::api::schema::{ComponentSettingKind, SettingsComponent};

use super::render::put_text;
use super::*;

fn display_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

/// Widths Enter steps through on the "Dock width" row.
const DOCK_WIDTHS: &[u16] = &[24, 32, 40, 48, 56, 64, 80];

pub(super) enum FilesRow {
    ShowDock,
    DockWidth,
    Component { component: usize, setting: usize },
}

impl ClientSettingsOverlay {
    /// The sections shown: Files only where the endpoint has it.
    pub(super) fn sections(&self) -> Vec<ClientSettingsSection> {
        ClientSettingsSection::ALL
            .iter()
            .copied()
            .filter(|section| *section != ClientSettingsSection::Files || self.files_available)
            .collect()
    }
}

pub(super) fn rows(settings: &ClientSettingsOverlay) -> Vec<FilesRow> {
    let mut rows = vec![FilesRow::ShowDock, FilesRow::DockWidth];
    for (component, entry) in settings.components.iter().enumerate() {
        rows.extend(
            (0..entry.settings.len()).map(|setting| FilesRow::Component { component, setting }),
        );
    }
    rows
}

fn value_text(kind: &ComponentSettingKind, value: &serde_json::Value) -> String {
    match (kind, value) {
        (ComponentSettingKind::Toggle, serde_json::Value::Bool(on)) => {
            if *on { "on" } else { "off" }.to_owned()
        }
        (_, serde_json::Value::String(text)) if text.is_empty() => "not set".to_owned(),
        (_, serde_json::Value::String(text)) => text.clone(),
        (_, other) => other.to_string(),
    }
}

impl ClientShellState {
    pub(super) fn active_endpoint_has_component_settings(&self) -> bool {
        self.endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == self.active_endpoint_id)
            .and_then(|endpoint| endpoint.methods.as_ref())
            .is_some_and(|methods| methods.contains("component_settings.list"))
    }

    /// Asks the endpoint for its component settings; with `set`, changes one
    /// first (the answer is the updated list either way).
    pub(super) fn queue_component_settings(
        &mut self,
        set: Option<crate::api::schema::ComponentSettingSetParams>,
        outcome: &mut ClientShellInput,
    ) {
        if let Some(ClientShellOverlay::Settings(settings)) = self.overlay.as_mut() {
            if !settings.files_available {
                return;
            }
            settings.loading_components = true;
            settings.component_message = None;
        }
        let method = match set {
            Some(params) => crate::api::schema::Method::ComponentSettingsSet(params),
            None => crate::api::schema::Method::ComponentSettingsList(
                crate::api::schema::EmptyParams::default(),
            ),
        };
        if !self.push_endpoint_method_with_kind(
            method,
            PendingEndpointKind::ComponentSettings,
            outcome,
        ) {
            if let Some(ClientShellOverlay::Settings(settings)) = self.overlay.as_mut() {
                settings.loading_components = false;
            }
        }
    }

    pub(super) fn receive_component_settings(
        &mut self,
        result: Result<crate::api::schema::ResponseResult, ClientShellEndpointError>,
    ) {
        let Some(ClientShellOverlay::Settings(settings)) = self.overlay.as_mut() else {
            return;
        };
        settings.loading_components = false;
        match result {
            Ok(crate::api::schema::ResponseResult::ComponentSettings { components }) => {
                settings.components = components;
                let count = rows(settings).len();
                settings.selected = settings.selected.min(count.saturating_sub(1));
            }
            Ok(_) => {
                settings.component_message = Some("the endpoint answered unexpectedly".into());
            }
            Err(error) => settings.component_message = Some(error.message),
        }
    }

    /// Enter on the selected row.
    pub(super) fn apply_files_setting(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::Settings(settings)) = self.overlay.as_mut() else {
            return;
        };
        if settings.loading_components {
            return;
        }
        let Some(row) = rows(settings).into_iter().nth(settings.selected) else {
            return;
        };
        match row {
            FilesRow::ShowDock => {
                self.toggle_dock(outcome);
                let visible = self.dock_visible;
                if let Some(ClientShellOverlay::Settings(settings)) = self.overlay.as_mut() {
                    settings.dock_visible = visible;
                }
            }
            FilesRow::DockWidth => {
                let next = DOCK_WIDTHS
                    .iter()
                    .copied()
                    .find(|width| *width > self.dock_width)
                    .unwrap_or(DOCK_WIDTHS[0]);
                self.dock_width = next;
                if let Some(ClientShellOverlay::Settings(settings)) = self.overlay.as_mut() {
                    settings.dock_width = next;
                }
                self.invalidate_pane_surface();
                self.persist_chrome_preferences(outcome);
                outcome.repaint = true;
            }
            FilesRow::Component { component, setting } => {
                let entry: &SettingsComponent = &settings.components[component];
                let item = &entry.settings[setting];
                if !item.enabled {
                    return;
                }
                let value = match &item.kind {
                    ComponentSettingKind::Toggle => {
                        serde_json::Value::Bool(!item.value.as_bool().unwrap_or(false))
                    }
                    ComponentSettingKind::Choice { options } => {
                        let current = item.value.as_str().unwrap_or_default();
                        let next = options
                            .iter()
                            .position(|option| option == current)
                            .map_or(0, |index| (index + 1) % options.len().max(1));
                        match options.get(next) {
                            Some(option) => serde_json::Value::String(option.clone()),
                            None => return,
                        }
                    }
                    ComponentSettingKind::Text => {
                        let text = item.value.as_str().unwrap_or_default();
                        settings.editing_text = Some((
                            entry.id.clone(),
                            item.key.clone(),
                            super::text_editor::TextEditor::new(text, false),
                        ));
                        outcome.repaint = true;
                        return;
                    }
                    ComponentSettingKind::Unknown => return,
                };
                let params = crate::api::schema::ComponentSettingSetParams {
                    component: entry.id.clone(),
                    key: item.key.clone(),
                    value,
                };
                self.queue_component_settings(Some(params), outcome);
                outcome.repaint = true;
            }
        }
    }

    /// Keys while a text setting is being typed: Enter saves, Esc leaves it.
    pub(super) fn route_files_text_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(ClientShellOverlay::Settings(settings)) = self.overlay.as_mut() else {
            return false;
        };
        let Some((_, _, input)) = settings.editing_text.as_mut() else {
            return false;
        };
        outcome.repaint = true;
        match key.code {
            crossterm::event::KeyCode::Esc => {
                settings.editing_text = None;
            }
            crossterm::event::KeyCode::Enter => {
                let Some((component, key, input)) = settings.editing_text.take() else {
                    return true;
                };
                let params = crate::api::schema::ComponentSettingSetParams {
                    component,
                    key,
                    value: serde_json::Value::String(input.as_str().trim().to_owned()),
                };
                self.queue_component_settings(Some(params), outcome);
            }
            _ => {
                input.handle_key(key);
            }
        }
        true
    }
}

/// Draws the Files section into `area`.
pub(super) fn render(
    buffer: &mut Buffer,
    area: Rect,
    settings: &ClientSettingsOverlay,
    palette: &Palette,
    hits: &mut Vec<(Rect, usize)>,
) {
    let (dock_visible, dock_width) = (settings.dock_visible, settings.dock_width);
    let title = Style::default()
        .fg(palette.text)
        .bg(palette.panel_bg)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(palette.overlay1).bg(palette.panel_bg);
    put_text(buffer, area.x, area.y, area.width, "files dock", title);
    let rows = rows(settings);
    let selected_help = match rows.get(settings.selected) {
        Some(FilesRow::ShowDock) => Some("the sidebar on the right of every tab (prefix+f)"),
        Some(FilesRow::DockWidth) => Some("or drag the line between the tabs and the dock"),
        Some(FilesRow::Component { component, setting }) => settings.components[*component]
            .settings[*setting]
            .help
            .as_deref(),
        None => None,
    };
    put_text(
        buffer,
        area.x,
        area.y + 1,
        area.width,
        selected_help.unwrap_or(""),
        dim,
    );
    let list = Rect::new(
        area.x,
        area.y + 3,
        area.width,
        area.height.saturating_sub(4),
    );
    let visible = usize::from(list.height);
    let scroll = settings.selected.saturating_sub(visible.saturating_sub(1));
    for (line, (index, row)) in rows
        .iter()
        .enumerate()
        .skip(scroll)
        .take(visible)
        .enumerate()
    {
        let rect = Rect::new(list.x, list.y + line as u16, list.width, 1);
        let (label, value, enabled) = match row {
            FilesRow::ShowDock => (
                "Show the files dock".to_owned(),
                if dock_visible { "on" } else { "off" }.to_owned(),
                true,
            ),
            FilesRow::DockWidth => ("Dock width".to_owned(), format!("{dock_width} cols"), true),
            FilesRow::Component { component, setting } => {
                let item = &settings.components[*component].settings[*setting];
                let editing = settings
                    .editing_text
                    .as_ref()
                    .filter(|(_, key, _)| *key == item.key)
                    .map(|(_, _, input)| format!("{}▏", input.as_str()));
                (
                    item.label.clone(),
                    editing.unwrap_or_else(|| value_text(&item.kind, &item.value)),
                    item.enabled && item.kind != ComponentSettingKind::Unknown,
                )
            }
        };
        let selected = index == settings.selected;
        let style = if selected {
            Style::default()
                .fg(contrast(palette))
                .bg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else if enabled {
            Style::default().fg(palette.text).bg(palette.panel_bg)
        } else {
            dim
        };
        buffer.set_style(rect, style);
        let marker = if selected { "▸" } else { " " };
        let left = format!(" {marker} {label}");
        let width = usize::from(rect.width);
        let pad = width.saturating_sub(display_width(&left) + display_width(&value) + 1);
        put_text(
            buffer,
            rect.x,
            rect.y,
            rect.width,
            &format!("{left}{}{value} ", " ".repeat(pad.max(1))),
            style,
        );
        hits.push((rect, index));
    }
    let status = if settings.loading_components {
        Some(" …".to_owned())
    } else {
        settings
            .component_message
            .as_ref()
            .map(|message| format!(" {message}"))
    };
    if let Some(status) = status {
        put_text(
            buffer,
            area.x,
            area.bottom().saturating_sub(1),
            area.width,
            &status,
            dim,
        );
    }
}

/// Text on the accent color (as the other settings sections draw it).
fn contrast(palette: &Palette) -> ratatui::style::Color {
    match palette.panel_bg {
        ratatui::style::Color::Reset => palette.surface_dim,
        color => color,
    }
}
