use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const STAGED_CLIPBOARD_IMAGE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

pub(crate) struct StagedClipboardImage {
    pub(crate) path: PathBuf,
    pub(crate) paste_text: String,
}

pub(crate) fn stage(
    client_id: u64,
    extension: &str,
    data: &[u8],
) -> io::Result<StagedClipboardImage> {
    let extension = sanitize_extension(extension);
    let dir = ensure_staging_dir()?;
    cleanup_stale(&dir);

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);

    for attempt in 0..100 {
        let path = dir.join(format!(
            "client-{client_id}-clipboard-{unique}-{attempt}.{extension}"
        ));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        restrict_file_options(&mut options);
        let mut file = match options.open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        };
        file.write_all(data)?;
        return Ok(StagedClipboardImage {
            paste_text: path.to_string_lossy().into_owned(),
            path,
        });
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "failed to allocate unique clipboard image staging path",
    ))
}

/// The name a clipboard image is sent under to another machine (hc keeps
/// it, so `.png` stays `.png`).
pub(crate) fn remote_name(client_id: u64, extension: &str) -> String {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!(
        "client-{client_id}-clipboard-{unique}.{}",
        sanitize_extension(extension)
    )
}

/// The command that removes clipboard images older than a day beside the
/// terminals of another machine, given where one was just put there
/// (`…/files/<terminal>/<name>`). Only herdr's own image names, only under
/// that `files` folder; `None` when the path does not look like one hc gave.
pub(crate) fn remote_cleanup_argv(placed: &str) -> Option<Vec<String>> {
    let placed = Path::new(placed);
    let name = placed.file_name()?.to_str()?;
    if !placed.is_absolute() || !name.starts_with("client-") || !name.contains("-clipboard-") {
        return None;
    }
    let files = placed.parent()?.parent()?;
    if files.file_name()? != "files" {
        return None;
    }
    Some(
        [
            "find",
            files.to_str()?,
            "-mindepth",
            "2",
            "-maxdepth",
            "2",
            "-type",
            "f",
            "-name",
            "client-*-clipboard-*",
            "-mmin",
            &format!("+{}", STAGED_CLIPBOARD_IMAGE_MAX_AGE.as_secs() / 60),
            "-delete",
        ]
        .iter()
        .map(|arg| (*arg).to_owned())
        .collect(),
    )
}

pub(crate) fn remove_files(paths: Vec<PathBuf>) {
    for path in paths {
        let _ = fs::remove_file(path);
    }
}

fn sanitize_extension(extension: &str) -> &'static str {
    if extension.eq_ignore_ascii_case("png") {
        "png"
    } else if extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg") {
        "jpg"
    } else if extension.eq_ignore_ascii_case("gif") {
        "gif"
    } else if extension.eq_ignore_ascii_case("webp") {
        "webp"
    } else if extension.eq_ignore_ascii_case("bmp") {
        "bmp"
    } else {
        "png"
    }
}

fn staging_dir() -> PathBuf {
    #[cfg(unix)]
    let user_id = unsafe { libc::geteuid() };
    #[cfg(windows)]
    let user_id = std::process::id();
    std::env::temp_dir().join(format!("herdr-clipboard-images-{user_id}"))
}

fn ensure_staging_dir() -> io::Result<PathBuf> {
    let dir = staging_dir();
    fs::create_dir_all(&dir)?;
    let metadata = fs::metadata(&dir)?;
    if !metadata.is_dir() {
        return Err(io::Error::other(format!(
            "clipboard image staging path is not a directory: {}",
            dir.display()
        )));
    }
    restrict_dir_permissions(&dir)?;
    Ok(dir)
}

#[cfg(unix)]
fn restrict_file_options(options: &mut fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;

    options.mode(0o600);
}

#[cfg(windows)]
fn restrict_file_options(_options: &mut fs::OpenOptions) {}

#[cfg(unix)]
fn restrict_dir_permissions(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

#[cfg(windows)]
fn restrict_dir_permissions(_dir: &Path) -> io::Result<()> {
    Ok(())
}

fn cleanup_stale(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if modified.elapsed().unwrap_or_default() > STAGED_CLIPBOARD_IMAGE_MAX_AGE {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn old_images_beside_another_machines_terminals_are_cleaned_and_nothing_else() {
        let argv = remote_cleanup_argv(
            "/home/u/.local/state/herdr-core/default/files/t3g/client-3-clipboard-17947526-0.png",
        )
        .expect("an hc put path");
        assert_eq!(argv[1], "/home/u/.local/state/herdr-core/default/files");
        assert!(argv
            .windows(2)
            .any(|w| w == ["-name", "client-*-clipboard-*"]));
        assert!(argv.windows(2).any(|w| w == ["-mmin", "+1440"]));
        assert_eq!(argv.last().map(String::as_str), Some("-delete"));
        // Not one of herdr's images, not under hc's files folder, or relative.
        assert!(
            remote_cleanup_argv("/home/u/.local/state/herdr-core/default/files/t3g/t1.png")
                .is_none()
        );
        assert!(remote_cleanup_argv("/home/u/Pictures/x/client-3-clipboard-1.png").is_none());
        assert!(remote_cleanup_argv("files/t3/client-3-clipboard-1.png").is_none());
    }

    use super::*;

    #[test]
    fn sanitize_extension_accepts_known_image_extensions() {
        assert_eq!(sanitize_extension("PNG"), "png");
        assert_eq!(sanitize_extension("jpeg"), "jpg");
        assert_eq!(sanitize_extension("webp"), "webp");
        assert_eq!(sanitize_extension("sh"), "png");
    }
}
