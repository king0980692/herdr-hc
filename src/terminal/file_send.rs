//! Sending dropped files beside a terminal on another machine, through its
//! hc core: up to [`MAX_FILE_BYTES`] each, in parts hc takes (`terminal.put`
//! for the first, `machine.exec` appending each next one), into a hidden
//! file that is renamed into a folder of its own only once whole. A program
//! there never sees a half file; the paths are pasted only after.

use std::io::Read as _;
use std::path::Path;

use base64::Engine as _;
use serde_json::json;

use crate::protocol::file_drop::{FileDropState, FileDropStatus, MAX_FILE_BYTES};

/// Bytes per call: under hc's 8 MB per `terminal.put` and `machine.exec`
/// input, with room for progress to move.
const PART_BYTES: usize = 4 * 1024 * 1024;

/// Appends its stdin to `$1`.
const APPEND: &str = r#"cat >> "$1""#;

/// `$1` the hidden file, `$2` the name to give it, `$3` the size it must
/// have. Moves it into a new folder beside it and prints its path there;
/// clears drops older than a day beside every terminal of that core.
const FINISH: &str = r#"set -e
part=$1; name=$2; size=$3
have=$(wc -c < "$part" | tr -d ' ')
if [ "$have" != "$size" ]; then
    rm -f "$part"
    echo "it arrived with $have of its $size bytes" >&2
    exit 3
fi
here=$(dirname "$part")
folder="$here/herdr-drop-$(date +%s)-$$"
mkdir "$folder"
mv "$part" "$folder/$name"
files=$(dirname "$here")
if [ "$(basename "$files")" = files ]; then
    find "$files" -mindepth 2 -maxdepth 2 \( -type d -name 'herdr-drop-*' -o -type f -name '.herdr-drop-*' \) -mmin +1440 -exec rm -rf {} + 2>/dev/null || true
fi
printf '%s' "$folder/$name""#;

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_000_000.0)
}

/// Why `path` cannot be sent, if it cannot.
pub(crate) fn refusal(path: &Path) -> Option<String> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    match std::fs::metadata(path) {
        Err(error) => Some(format!("{name}: {error}")),
        Ok(meta) if meta.is_dir() => Some(format!("{name} is a folder; folders are not sent yet")),
        Ok(meta) if !meta.is_file() => Some(format!("{name} is not a regular file")),
        Ok(meta) if meta.len() > MAX_FILE_BYTES => Some(format!(
            "{name} is {}; the most sent is {}",
            megabytes(meta.len()),
            megabytes(MAX_FILE_BYTES)
        )),
        Ok(_) => None,
    }
}

async fn exec(client: &crate::hc::Client, argv: &[&str], input: &[u8]) -> Result<Vec<u8>, String> {
    let reply = client
        .call(
            "machine.exec",
            json!({
                "argv": argv,
                "input": base64::engine::general_purpose::STANDARD.encode(input),
                "timeout": 120,
            }),
        )
        .await
        .map_err(|error| error.to_string())?;
    let decode = |field: &str| {
        base64::engine::general_purpose::STANDARD
            .decode(reply[field].as_str().unwrap_or_default())
            .unwrap_or_default()
    };
    if reply["code"] == 0 {
        Ok(decode("stdout"))
    } else {
        let stderr = String::from_utf8_lossy(&decode("stderr")).trim().to_owned();
        Err(if stderr.is_empty() {
            format!("it ended with {}", reply["code"])
        } else {
            stderr
        })
    }
}

/// Sends one file; its path there.
async fn send_one(
    client: &crate::hc::Client,
    address: &str,
    path: &Path,
    report: &(impl Fn(FileDropStatus) + Send + Sync),
) -> Result<String, String> {
    if let Some(refusal) = refusal(path) {
        return Err(refusal);
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(|| format!("{} has no name", path.display()))?;
    let mut file = std::fs::File::open(path).map_err(|error| format!("{name}: {error}"))?;
    let size = file
        .metadata()
        .map_err(|error| format!("{name}: {error}"))?
        .len();
    let mut part = vec![0; PART_BYTES];
    let mut read = |part: &mut Vec<u8>| -> Result<usize, String> {
        let mut filled = 0;
        while filled < part.len() {
            match file.read(&mut part[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(error) => return Err(format!("{name}: {error}")),
            }
        }
        Ok(filled)
    };
    let progress = |sent: u64| {
        if size > PART_BYTES as u64 {
            report(FileDropStatus {
                state: FileDropState::Sending,
                title: format!("Sending {name}"),
                detail: format!("{}% of {}", sent * 100 / size.max(1), megabytes(size)),
            });
        }
    };
    progress(0);
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let first = read(&mut part)?;
    let reply = client
        .call(
            "terminal.put",
            json!({
                "terminal": address,
                "name": format!(".herdr-drop-{unique}.partial"),
                "data": base64::engine::general_purpose::STANDARD.encode(&part[..first]),
            }),
        )
        .await
        .map_err(|error| format!("{name}: {error}"))?;
    let hidden = reply["path"]
        .as_str()
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("{name}: hc put returned no path"))?;
    let mut sent = first as u64;
    let rest = async {
        loop {
            progress(sent);
            let n = read(&mut part)?;
            if n == 0 {
                break;
            }
            exec(client, &["sh", "-c", APPEND, "sh", &hidden], &part[..n]).await?;
            sent += n as u64;
        }
        let placed = exec(
            client,
            &["sh", "-c", FINISH, "sh", &hidden, &name, &sent.to_string()],
            &[],
        )
        .await?;
        String::from_utf8(placed)
            .ok()
            .filter(|placed| !placed.is_empty())
            .ok_or_else(|| "it gave no path back".to_owned())
    }
    .await;
    match rest {
        Ok(placed) => Ok(placed),
        Err(error) => {
            let _ = exec(client, &["rm", "-f", &hidden], &[]).await;
            Err(format!("{name}: {error}"))
        }
    }
}

/// Sends `paths` beside the terminal `address`, in order; their paths there.
/// Stops at the first that cannot be sent (those sent before stay, unpasted,
/// and are cleared a day later).
pub(crate) async fn send_files(
    client: &crate::hc::Client,
    address: &str,
    paths: &[std::path::PathBuf],
    report: impl Fn(FileDropStatus) + Send + Sync,
) -> Result<Vec<String>, String> {
    if let Some(refusal) = paths.iter().find_map(|path| refusal(path)) {
        return Err(refusal);
    }
    let mut placed = Vec::with_capacity(paths.len());
    for path in paths {
        placed.push(send_one(client, address, path, &report).await?);
    }
    Ok(placed)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    struct Dir(std::path::PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn scratch() -> Dir {
        let root = std::env::temp_dir().join(format!(
            "herdr-file-send-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        Dir(root)
    }

    fn sh(script: &str, args: &[&str], input: &[u8]) -> (bool, String, String) {
        use std::io::Write as _;
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .arg("sh")
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        let out = child.wait_with_output().unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    #[test]
    fn parts_join_into_a_file_that_appears_whole_in_a_folder_of_its_own() {
        let root = scratch();
        let terminal = root.0.join("files").join("t7");
        std::fs::create_dir_all(&terminal).unwrap();
        let hidden = terminal.join(".herdr-drop-1.partial");
        std::fs::write(&hidden, b"first ").unwrap();
        let hidden = hidden.to_str().unwrap();
        assert!(sh(APPEND, &[hidden], b"second").0);

        // Not whole: removed, nothing placed.
        let (ok, _, stderr) = sh(FINISH, &[hidden, "My Report.pdf", "99"], b"");
        assert!(!ok);
        assert!(stderr.contains("12 of its 99"), "{stderr}");
        assert!(!Path::new(hidden).exists());

        // Whole: moved under its own name into a new folder; its path printed.
        std::fs::write(hidden, b"first second").unwrap();
        // A drop from two days ago beside another terminal is cleared.
        let old = root.0.join("files").join("t2").join("herdr-drop-1-1");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("a.txt"), b"old").unwrap();
        assert!(
            sh(
                "touch -t 202001010000 \"$1\"",
                &[old.to_str().unwrap()],
                b""
            )
            .0
        );
        let (ok, placed, stderr) = sh(FINISH, &[hidden, "My Report.pdf", "12"], b"");
        assert!(ok, "{stderr}");
        let placed = Path::new(&placed);
        assert_eq!(placed.file_name().unwrap(), "My Report.pdf");
        assert_eq!(placed.parent().unwrap().parent().unwrap(), terminal);
        assert!(placed
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("herdr-drop-"));
        assert_eq!(std::fs::read(placed).unwrap(), b"first second");
        assert!(!Path::new(hidden).exists());
        assert!(!old.exists(), "a day-old drop is cleared");
    }

    #[test]
    fn folders_missing_and_oversized_files_are_refused_by_name() {
        let root = scratch();
        let small = root.0.join("small.txt");
        std::fs::write(&small, b"x").unwrap();
        assert_eq!(refusal(&small), None);
        assert!(refusal(&root.0)
            .unwrap()
            .contains("folders are not sent yet"));
        assert!(refusal(&root.0.join("gone.txt"))
            .unwrap()
            .starts_with("gone.txt: "));
        let big = root.0.join("big.bin");
        let file = std::fs::File::create(&big).unwrap();
        file.set_len(MAX_FILE_BYTES + 1).unwrap();
        let refused = refusal(&big).unwrap();
        assert!(refused.starts_with("big.bin is 268.4 MB"), "{refused}");
    }
}
