//! Optional session terminal: one terminal of the whole session, outside
//! every tab (the files dock's program), shown by clients beside their tabs.
//!
//! Negotiated apart from the generation-1 core: the server advertises
//! [`CAPABILITY`]; a client that wants it sends [`VIEW_KIND`] with the cells
//! it shows the terminal in, and from then on receives [`UPDATE_KIND`].
//! Input reuses `ClientShellPopupInput`, addressed by the terminal's id.

use base64::{engine::general_purpose::STANDARD_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};

use super::{FrameData, ServerMessage};

pub const CAPABILITY: &str = "session_terminal";

static OFFERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// This server runs a session terminal: its welcome advertises it.
pub fn offer() {
    OFFERED.store(true, std::sync::atomic::Ordering::Release);
}

pub fn offered() -> bool {
    OFFERED.load(std::sync::atomic::Ordering::Acquire)
}
/// Server → client: the terminal's screen (binary layout below is frozen
/// with this name).
pub const UPDATE_KIND: &str = "endpoint.session-terminal.v1";
/// Client → server: how this client shows the terminal (JSON).
pub const VIEW_KIND: &str = "endpoint.session-terminal.view.v1";

/// How one client shows the session terminal. JSON; absent fields default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTerminalView {
    #[serde(default)]
    pub visible: bool,
    #[serde(default)]
    pub cols: u16,
    #[serde(default)]
    pub rows: u16,
    #[serde(default)]
    pub focused: bool,
}

// This binary layout is frozen with UPDATE_KIND, independently of the v1 core.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionTerminalUpdate {
    pub terminal_id: String,
    /// What the terminal is for (`files`).
    pub role: String,
    pub status: SessionTerminalStatus,
    pub mouse_reporting: bool,
    pub sgr_pixel_mouse: bool,
    /// Its screen; `None` while it is not running.
    pub frame: Option<FrameData>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionTerminalStatus {
    Running,
    /// Starting again after it exited.
    Restarting,
    /// It kept exiting; a view asking for it again restarts it.
    Failed,
}

pub fn update_message(update: &SessionTerminalUpdate) -> Result<ServerMessage, String> {
    let bytes = bincode::serde::encode_to_vec(update, bincode::config::standard())
        .map_err(|error| error.to_string())?;
    Ok(ServerMessage::EndpointControl {
        kind: UPDATE_KIND.into(),
        data: STANDARD_NO_PAD.encode(bytes),
    })
}

pub fn decode_update(data: &str) -> Result<SessionTerminalUpdate, String> {
    let bytes = STANDARD_NO_PAD
        .decode(data)
        .map_err(|error| format!("session terminal update is not base64: {error}"))?;
    let (update, read): (SessionTerminalUpdate, usize) =
        bincode::serde::decode_from_slice(&bytes, bincode::config::standard())
            .map_err(|error| format!("session terminal update does not decode: {error}"))?;
    if read != bytes.len() {
        return Err("session terminal update has trailing bytes".into());
    }
    if let Some(frame) = &update.frame {
        if frame.cells.len() != usize::from(frame.width) * usize::from(frame.height) {
            return Err("session terminal frame size disagrees with its cells".into());
        }
    }
    Ok(update)
}

pub fn view_message(view: SessionTerminalView) -> super::ClientMessage {
    super::ClientMessage::EndpointControl {
        kind: VIEW_KIND.into(),
        data: serde_json::to_string(&view).unwrap_or_default(),
    }
}

pub fn decode_view(data: &str) -> Result<SessionTerminalView, String> {
    serde_json::from_str(data).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{buffer::Buffer, layout::Rect};

    fn update() -> SessionTerminalUpdate {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 2));
        buffer[(0, 0)].set_symbol("x");
        SessionTerminalUpdate {
            terminal_id: "term_1".into(),
            role: "files".into(),
            status: SessionTerminalStatus::Running,
            mouse_reporting: true,
            sgr_pixel_mouse: false,
            frame: Some(FrameData::from_ratatui_buffer(&buffer, None)),
        }
    }

    #[test]
    fn updates_round_trip_and_bad_frames_are_rejected() {
        let ServerMessage::EndpointControl { kind, data } = update_message(&update()).unwrap()
        else {
            panic!("an endpoint control");
        };
        assert_eq!(kind, UPDATE_KIND);
        assert_eq!(decode_update(&data).unwrap(), update());

        let mut bad = update();
        bad.frame.as_mut().unwrap().cells.pop();
        let ServerMessage::EndpointControl { data, .. } = update_message(&bad).unwrap() else {
            panic!("an endpoint control");
        };
        assert!(decode_update(&data).is_err());
        assert!(decode_update("not base64!").is_err());
    }

    #[test]
    fn views_default_missing_fields_and_ignore_unknown_ones() {
        assert_eq!(decode_view("{}").unwrap(), SessionTerminalView::default());
        assert_eq!(
            decode_view(r#"{"visible":true,"cols":32,"rows":40,"focused":false,"later":1}"#)
                .unwrap(),
            SessionTerminalView {
                visible: true,
                cols: 32,
                rows: 40,
                focused: false
            }
        );
    }

    #[test]
    fn session_terminal_v1_binary_layout_is_frozen() {
        use sha2::{Digest, Sha256};
        let ServerMessage::EndpointControl { data, .. } = update_message(&update()).unwrap() else {
            panic!("an endpoint control");
        };
        let digest = format!("{:x}", Sha256::digest(data.as_bytes()));
        assert_eq!(digest, digest_of_frozen_layout(), "{data}");
    }

    fn digest_of_frozen_layout() -> &'static str {
        "fa1f645e90a09b83d4b53143f01dd4ad8c9d93e13bf181a55200da573de7f239"
    }
}
