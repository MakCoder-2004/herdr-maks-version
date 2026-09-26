use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const LARGE_PASTE_MIN_BYTES: usize = 8 * 1024;
const LARGE_PASTE_MIN_LINES: usize = 100;
const STAGED_PASTE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

struct StagedClipboardText {
    path: PathBuf,
    line_count: usize,
}

pub(crate) fn compact_large_paste(text: String, cwd: Option<&Path>) -> (String, Option<PathBuf>) {
    if !is_large_paste(&text) {
        return (text, None);
    }

    match stage(&text, cwd) {
        Ok(staged) => (reference(&staged), Some(staged.path)),
        Err(error) => {
            tracing::warn!(
                %error,
                "could not stage large clipboard paste; forwarding original text"
            );
            (text, None)
        }
    }
}

fn is_large_paste(text: &str) -> bool {
    text.len() >= LARGE_PASTE_MIN_BYTES || line_count(text) >= LARGE_PASTE_MIN_LINES
}

fn line_count(text: &str) -> usize {
    let mut lines = 0;
    let mut previous_was_carriage_return = false;
    let mut ends_with_line_break = false;
    for character in text.chars() {
        match character {
            '\r' => {
                lines += 1;
                previous_was_carriage_return = true;
                ends_with_line_break = true;
            }
            '\n' => {
                if !previous_was_carriage_return {
                    lines += 1;
                }
                previous_was_carriage_return = false;
                ends_with_line_break = true;
            }
            _ => {
                previous_was_carriage_return = false;
                ends_with_line_break = false;
            }
        }
    }
    if !text.is_empty() && !ends_with_line_break {
        lines += 1;
    }
    lines
}

fn reference(staged: &StagedClipboardText) -> String {
    let unit = if staged.line_count == 1 {
        "line"
    } else {
        "lines"
    };
    format!(
        "[pasted {} {unit}; read \"{}\"]",
        staged.line_count,
        staged.path.display()
    )
}

fn stage(text: &str, cwd: Option<&Path>) -> io::Result<StagedClipboardText> {
    if let Some(cwd) = cwd {
        match stage_to_directory(text, &cwd.join(".herdr-pastes")) {
            Ok(staged) => return Ok(staged),
            Err(error) => {
                tracing::debug!(
                    %error,
                    "workspace paste staging failed; using Herdr state directory"
                );
            }
        }
    }

    let state_directory = crate::config::state_dir();
    fs::create_dir_all(&state_directory)?;
    stage_to_directory(text, &state_directory.join("pastes"))
}

pub(crate) fn latest_staged_paste(cwd: Option<&Path>) -> Option<PathBuf> {
    cwd.map(|cwd| cwd.join(".herdr-pastes"))
        .into_iter()
        .chain(std::iter::once(crate::config::state_dir().join("pastes")))
        .filter_map(|directory| latest_staged_paste_in_directory(&directory))
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, path)| path)
}

fn latest_staged_paste_in_directory(directory: &Path) -> Option<(SystemTime, PathBuf)> {
    fs::read_dir(directory)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            if !entry.file_name().to_string_lossy().starts_with("paste-") {
                return None;
            }
            let file_type = entry.file_type().ok()?;
            if !file_type.is_file() || file_type.is_symlink() {
                return None;
            }
            let path = entry.path();
            let modified = entry.metadata().ok()?.modified().ok()?;
            (modified.elapsed().unwrap_or_default() <= STAGED_PASTE_MAX_AGE)
                .then_some((modified, path))
        })
        .max_by_key(|(modified, _)| *modified)
}

fn stage_to_directory(text: &str, directory: &Path) -> io::Result<StagedClipboardText> {
    match crate::platform::create_private_state_directory(directory) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = fs::symlink_metadata(directory)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "clipboard paste staging path is not a directory",
        ));
    }
    cleanup_stale(directory);

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    for attempt in 0..100 {
        let path = directory.join(format!("paste-{unique}-{attempt}.txt"));
        let mut file = match crate::platform::create_private_state_file(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        if let Err(error) = file.write_all(text.as_bytes()) {
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        return Ok(StagedClipboardText {
            path,
            line_count: line_count(text).max(1),
        });
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "failed to allocate unique clipboard paste file",
    ))
}

fn cleanup_stale(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with("paste-") {
            continue;
        }
        let path = entry.path();
        let Ok(modified) = entry.metadata().and_then(|metadata| metadata.modified()) else {
            continue;
        };
        if modified.elapsed().unwrap_or_default() > STAGED_PASTE_MAX_AGE {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_directory() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("herdr-paste-test-{}-{unique}", std::process::id()))
    }

    #[test]
    fn large_paste_is_staged_verbatim_and_replaced_with_a_single_line_reference() {
        let directory = test_directory();
        fs::create_dir_all(&directory).expect("create test staging directory");
        let original = (0..LARGE_PASTE_MIN_LINES)
            .map(|line| format!("line {line}\r\n"))
            .collect::<String>();

        assert!(is_large_paste(&original));
        let staged = stage_to_directory(&original, &directory).expect("stage text");
        let reference = reference(&staged);

        assert_eq!(staged.line_count, LARGE_PASTE_MIN_LINES);
        assert!(reference.starts_with("[pasted 100 lines; read \""));
        assert!(!reference.contains(['\r', '\n']));
        assert_eq!(
            fs::read_to_string(&staged.path).expect("read staged text"),
            original
        );

        fs::remove_dir_all(directory).expect("remove test staging directory");
    }

    #[test]
    fn compact_large_paste_returns_the_staged_file_for_review() {
        let cwd = test_directory();
        fs::create_dir_all(&cwd).expect("create test workspace");
        let original = (0..LARGE_PASTE_MIN_LINES)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();

        let (reference, path) = compact_large_paste(original.clone(), Some(&cwd));
        let path = path.expect("large paste path");

        assert!(reference.contains(&path.display().to_string()));
        assert_eq!(
            fs::read_to_string(&path).expect("read staged paste"),
            original
        );
        fs::remove_dir_all(cwd).expect("remove test workspace");
    }

    #[test]
    fn latest_staged_paste_finds_a_recent_workspace_file() {
        let cwd = test_directory();
        let directory = cwd.join(".herdr-pastes");
        fs::create_dir_all(&directory).expect("create test staging directory");
        let staged = stage_to_directory("saved paste", &directory).expect("stage text");

        assert_eq!(
            latest_staged_paste_in_directory(&directory).map(|(_, path)| path),
            Some(staged.path)
        );

        fs::remove_dir_all(cwd).expect("remove test workspace");
    }

    #[test]
    fn small_pastes_remain_unchanged() {
        let text = "keep this paste exactly as entered".to_owned();
        assert_eq!(compact_large_paste(text.clone(), None).0, text);
    }

    #[test]
    fn line_count_handles_lf_crlf_and_cr_separators() {
        assert_eq!(line_count("one\ntwo\nthree"), 3);
        assert_eq!(line_count("one\r\ntwo\r\nthree\r\n"), 3);
        assert_eq!(line_count("one\rtwo\rthree"), 3);
    }
}
