//! Optional file drop: files dropped on a pane whose program runs on another
//! machine are sent there, and their paths there are pasted.
//!
//! Negotiated apart from the generation-1 core: a server that reaches other
//! machines advertises [`CAPABILITY`]. A client on the same machine as that
//! server (it names files by their local paths) sends [`REQUEST_KIND`]; the
//! server answers with [`STATUS_KIND`] while sending, and when done or failed.
//! Both are JSON; absent fields default and unknown states are ignored.

use serde::{Deserialize, Serialize};

use super::{ClientClipboardImageTarget, ClientMessage, ServerMessage};

pub const CAPABILITY: &str = "file_drop";
/// Client → server: the files to send.
pub const REQUEST_KIND: &str = "endpoint.file-drop.v1";
/// Server → client: how sending goes.
pub const STATUS_KIND: &str = "endpoint.file-drop.status.v1";
/// The largest file sent.
pub const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;

static OFFERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// This server sends dropped files to other machines: its welcome says so.
pub fn offer() {
    OFFERED.store(true, std::sync::atomic::Ordering::Release);
}

pub fn offered() -> bool {
    OFFERED.load(std::sync::atomic::Ordering::Acquire)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDropRequest {
    /// Where the paths are pasted (as a clipboard image's).
    pub target: ClientClipboardImageTarget,
    /// Local paths of regular files, in the order dropped.
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileDropState {
    Sending,
    Done,
    Failed,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDropStatus {
    pub state: FileDropState,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub detail: String,
}

pub fn request_message(request: &FileDropRequest) -> ClientMessage {
    ClientMessage::EndpointControl {
        kind: REQUEST_KIND.into(),
        data: serde_json::to_string(request).unwrap_or_default(),
    }
}

pub fn decode_request(data: &str) -> Result<FileDropRequest, String> {
    serde_json::from_str(data).map_err(|error| error.to_string())
}

pub fn status_message(status: &FileDropStatus) -> ServerMessage {
    ServerMessage::EndpointControl {
        kind: STATUS_KIND.into(),
        data: serde_json::to_string(status).unwrap_or_default(),
    }
}

pub fn decode_status(data: &str) -> Result<FileDropStatus, String> {
    serde_json::from_str(data).map_err(|error| error.to_string())
}

/// What a terminal's drop (or paste) of `text` holds: `None` when it is not
/// only paths of existing files and folders. Paths are space separated,
/// each bare with backslash escapes (`My\ File.pdf`) or quoted.
pub fn dropped_paths(text: &str) -> Option<Vec<std::path::PathBuf>> {
    let text = text
        .strip_prefix("\x1b[200~")
        .and_then(|text| text.strip_suffix("\x1b[201~"))
        .unwrap_or(text)
        .trim();
    if text.is_empty() || text.contains(['\n', '\r']) {
        return None;
    }
    let mut paths = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                current.push(chars.next()?);
                started = true;
            }
            '\'' | '"' => {
                let quote = c;
                loop {
                    match chars.next()? {
                        c if c == quote => break,
                        '\\' if quote == '"' => current.push(chars.next()?),
                        c => current.push(c),
                    }
                }
                started = true;
            }
            c if c.is_whitespace() => {
                if started {
                    paths.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            c => {
                current.push(c);
                started = true;
            }
        }
    }
    if started {
        paths.push(current);
    }
    let paths: Vec<std::path::PathBuf> = paths.into_iter().map(Into::into).collect();
    (!paths.is_empty()
        && paths
            .iter()
            .all(|path| path.is_absolute() && path.symlink_metadata().is_ok()))
    .then_some(paths)
}

/// `path` as one word of a shell line, escaped the way terminals write a
/// dropped path.
pub fn shell_word(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        if !(c.is_alphanumeric() || "/._-+,@%:=~".contains(c)) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_statuses_round_trip_and_unknown_states_are_kept_apart() {
        let request = FileDropRequest {
            target: ClientClipboardImageTarget::Pane("w1:p2".into()),
            paths: vec!["/a b/c.pdf".into()],
        };
        let ClientMessage::EndpointControl { kind, data } = request_message(&request) else {
            panic!("an endpoint control");
        };
        assert_eq!(kind, REQUEST_KIND);
        assert_eq!(decode_request(&data).unwrap(), request);

        let status = FileDropStatus {
            state: FileDropState::Sending,
            title: "Sending c.pdf".into(),
            detail: "40%".into(),
        };
        let ServerMessage::EndpointControl { kind, data } = status_message(&status) else {
            panic!("an endpoint control");
        };
        assert_eq!(kind, STATUS_KIND);
        assert_eq!(decode_status(&data).unwrap(), status);
        // A newer server's state: ignorable, the rest still reads.
        let newer = decode_status(r#"{"state":"paused","title":"t"}"#).unwrap();
        assert_eq!(newer.state, FileDropState::Unknown);
        assert_eq!(newer.detail, "");
    }

    /// A folder removed with the test.
    struct Dir(std::path::PathBuf);
    impl Dir {
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn dropped_text_is_read_as_paths_only_when_every_one_exists() {
        let root = std::env::temp_dir().join(format!(
            "herdr-file-drop-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let dir = Dir(root);
        let plain = dir.path().join("a.txt");
        let spaced = dir.path().join("My Report.pdf");
        let quoted = dir.path().join("it's.csv");
        for path in [&plain, &spaced, &quoted] {
            std::fs::write(path, b"x").unwrap();
        }
        let p = |path: &std::path::Path| path.display().to_string();
        let escaped = |path: &std::path::Path| shell_word(&p(path));

        // One path, bare or inside a bracketed paste.
        assert_eq!(dropped_paths(&p(&plain)), Some(vec![plain.clone()]));
        assert_eq!(
            dropped_paths(&format!("\x1b[200~{} \x1b[201~", p(&plain))),
            Some(vec![plain.clone()])
        );
        // Several, as terminals write them: escaped or quoted.
        assert_eq!(
            dropped_paths(&format!(
                "{} {} '{}'",
                escaped(&plain),
                escaped(&spaced),
                p(&spaced)
            )),
            Some(vec![plain.clone(), spaced.clone(), spaced.clone()])
        );
        assert_eq!(
            dropped_paths(&format!("\"{}\" {}", p(&quoted), escaped(&quoted))),
            Some(vec![quoted.clone(), quoted.clone()])
        );
        // A folder is a path too (what to do with it is the caller's).
        assert_eq!(dropped_paths(&p(dir.path())), Some(vec![dir.path().into()]));

        // Anything else is text: a missing path, a relative one, words,
        // lines, an unfinished quote or escape.
        let missing = dir.path().join("missing.txt");
        assert_eq!(
            dropped_paths(&format!("{} {}", p(&plain), p(&missing))),
            None
        );
        assert_eq!(dropped_paths("a.txt"), None);
        assert_eq!(dropped_paths("hello world"), None);
        assert_eq!(
            dropped_paths(&format!("{}\n{}", p(&plain), p(&plain))),
            None
        );
        assert_eq!(dropped_paths(&format!("'{}", p(&plain))), None);
        assert_eq!(dropped_paths(&format!("{}\\", p(&plain))), None);
        assert_eq!(dropped_paths("   "), None);
    }

    #[test]
    fn shell_words_escape_what_a_shell_would_split_or_expand() {
        assert_eq!(shell_word("/a/b-c_d.1+2,x@y%z:=~"), "/a/b-c_d.1+2,x@y%z:=~");
        assert_eq!(shell_word("/a/My Report.pdf"), "/a/My\\ Report.pdf");
        assert_eq!(shell_word("/a/it's (1)$x"), "/a/it\\'s\\ \\(1\\)\\$x");
        assert_eq!(shell_word("/a/報告.pdf"), "/a/報告.pdf");
    }
}
