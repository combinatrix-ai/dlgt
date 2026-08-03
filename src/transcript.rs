//! Bounded Claude transcript fallback for a missing hook `final_text`.
//!
//! The Stop hook is authoritative, but it can arrive without
//! `last_assistant_message`, which used to be serialized as an empty string
//! and reported as a completed execution with no answer. The transcript is a
//! provider-written file, so it is untrusted input: the path is canonicalized,
//! confined to the Claude projects directory, required to name this Session,
//! and read only from the byte boundary recorded when the execution was
//! accepted, so a previous turn's answer can never be returned.

use std::io::{Read as _, Seek as _, SeekFrom};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Bytes read back from the end of a transcript.
pub const TAIL_LIMIT: u64 = 2 * 1024 * 1024;

/// Byte offset recorded when an execution is accepted. Only assistant
/// messages written after it belong to that execution.
///
/// Accepted residual, recorded in docs/design.md: this lookup is unconfined
/// and binds to a path rather than to a file identity, so a replaced file
/// could pair a stale offset with new content. The provider that writes the
/// transcript runs as the same user and can already modify it directly.
pub fn boundary(path: &str) -> Option<u64> {
    std::fs::metadata(path).ok().map(|meta| meta.len())
}

/// Recover the final assistant text for one execution, or `None`.
pub fn recover(path: &str, provider_session_id: &str, boundary: u64) -> Option<String> {
    let root = projects_root()?;
    let resolved = resolve(path, &root, provider_session_id)?;
    let mut file = open_checked(&resolved)?;
    let length = file.metadata().ok()?.len();
    if length <= boundary {
        return None;
    }
    let start = boundary.max(length.saturating_sub(TAIL_LIMIT));
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut tail = Vec::new();
    file.take(TAIL_LIMIT).read_to_end(&mut tail).ok()?;
    final_text(&tail, start > boundary)
}

/// Open the exact file the confinement check validated.
///
/// Canonicalization happens before the open, so the final component is
/// reopened with `O_NOFOLLOW` and the opened descriptor is compared against
/// the checked target by device and inode. A regular file is required.
/// Swapping an intermediate directory between the two steps is not covered;
/// closing it needs an `openat2(RESOLVE_BENEATH)`-style walk, which is not
/// portable here. Accepted residual: the transcript is written by a same-user
/// provider that can already modify the target directly, so the race grants no
/// privilege it does not already have. Recorded in docs/design.md.
fn open_checked(resolved: &Path) -> Option<std::fs::File> {
    let expected = std::fs::symlink_metadata(resolved).ok()?;
    if !expected.is_file() {
        return None;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(resolved)
        .ok()?;
    let opened = file.metadata().ok()?;
    if !opened.is_file() || opened.dev() != expected.dev() || opened.ino() != expected.ino() {
        return None;
    }
    Some(file)
}

fn projects_root() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    PathBuf::from(home)
        .join(".claude/projects")
        .canonicalize()
        .ok()
}

/// Confine an untrusted transcript path to one Session's own transcript.
/// Canonicalization resolves symlinks, so an escaping link fails the prefix
/// check rather than being followed.
fn resolve(path: &str, root: &Path, provider_session_id: &str) -> Option<PathBuf> {
    if path.is_empty() {
        return None;
    }
    let canonical = Path::new(path).canonicalize().ok()?;
    if !canonical.starts_with(root) {
        return None;
    }
    if canonical.extension()? != "jsonl" {
        return None;
    }
    if canonical.file_stem()? != provider_session_id {
        return None;
    }
    Some(canonical)
}

/// Last assistant text in a transcript slice. `partial_head` drops a first
/// line that the tail limit cut in half.
pub fn final_text(bytes: &[u8], partial_head: bool) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let mut lines = text.lines();
    if partial_head {
        lines.next();
    }
    lines
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| entry.get("type").and_then(Value::as_str) == Some("assistant"))
        .filter_map(|entry| assistant_text(&entry))
        .rfind(|text| !text.is_empty())
}

fn assistant_text(entry: &Value) -> Option<String> {
    let content = entry.pointer("/message/content")?;
    if let Some(text) = content.as_str() {
        return Some(text.to_owned());
    }
    let joined = content
        .as_array()?
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    Some(joined)
}

#[cfg(test)]
mod tests {
    use super::{boundary, final_text, recover, resolve};

    fn transcript(session: &str, messages: &[&str]) -> String {
        messages
            .iter()
            .map(|text| {
                format!(
                    r#"{{"type":"assistant","sessionId":"{session}","message":{{"role":"assistant","content":[{{"type":"text","text":"{text}"}}]}}}}"#
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    }

    #[test]
    fn the_newest_assistant_message_wins() {
        let bytes = transcript("s", &["first", "second"]);
        assert_eq!(
            final_text(bytes.as_bytes(), false).as_deref(),
            Some("second")
        );
    }

    #[test]
    fn a_partial_first_line_is_dropped() {
        let bytes = "not json at all\n".to_owned() + &transcript("s", &["kept"]);
        assert_eq!(final_text(bytes.as_bytes(), true).as_deref(), Some("kept"));
        assert!(final_text(b"", false).is_none());
    }

    #[test]
    fn non_assistant_and_empty_entries_are_ignored() {
        let bytes = concat!(
            r#"{"type":"user","message":{"content":[{"type":"text","text":"prompt"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"answer"}]}}"#,
            "\n",
            r#"{"type":"system","message":{"content":"noise"}}"#,
            "\n",
        );
        assert_eq!(
            final_text(bytes.as_bytes(), false).as_deref(),
            Some("answer")
        );
    }

    #[test]
    fn only_messages_after_the_acceptance_boundary_are_recovered()
    -> Result<(), Box<dyn std::error::Error>> {
        let home = tempfile::tempdir()?;
        let projects = home.path().join(".claude/projects/repo");
        std::fs::create_dir_all(&projects)?;
        let path = projects.join("session-1.jsonl");
        std::fs::write(&path, transcript("session-1", &["previous turn"]))?;
        let path_text = path.to_string_lossy().into_owned();

        // SAFETY: the test process sets HOME before the single-threaded read
        // below and does not rely on the previous value.
        unsafe { std::env::set_var("HOME", home.path()) };
        let accepted = boundary(&path_text).unwrap_or_else(|| panic!("boundary missing"));
        assert!(recover(&path_text, "session-1", accepted).is_none());

        std::fs::write(
            &path,
            transcript("session-1", &["previous turn", "this turn"]),
        )?;
        assert_eq!(
            recover(&path_text, "session-1", accepted).as_deref(),
            Some("this turn")
        );
        Ok(())
    }

    #[test]
    fn a_symlinked_transcript_is_never_opened() -> Result<(), Box<dyn std::error::Error>> {
        let home = tempfile::tempdir()?;
        let target = home.path().join("secret.jsonl");
        std::fs::write(&target, "{}\n")?;
        let link = home.path().join("link.jsonl");
        std::os::unix::fs::symlink(&target, &link)?;

        assert!(super::open_checked(&link).is_none());
        assert!(super::open_checked(&target).is_some());
        assert!(super::open_checked(home.path()).is_none());
        Ok(())
    }

    #[test]
    fn a_path_outside_the_projects_directory_is_rejected() -> Result<(), Box<dyn std::error::Error>>
    {
        let home = tempfile::tempdir()?;
        let root = home.path().join("projects");
        let outside = home.path().join("outside");
        std::fs::create_dir_all(&root)?;
        std::fs::create_dir_all(&outside)?;
        let secret = outside.join("session-1.jsonl");
        std::fs::write(&secret, "{}\n")?;
        let inside = root.join("session-1.jsonl");
        std::fs::write(&inside, "{}\n")?;
        let root = root.canonicalize()?;

        assert!(resolve(&secret.to_string_lossy(), &root, "session-1").is_none());
        assert!(resolve(&inside.to_string_lossy(), &root, "other").is_none());
        assert!(resolve(&inside.to_string_lossy(), &root, "session-1").is_some());

        std::os::unix::fs::symlink(&secret, root.join("escape.jsonl"))?;
        assert!(
            resolve(
                &root.join("escape.jsonl").to_string_lossy(),
                &root,
                "escape"
            )
            .is_none(),
            "a symlink out of the projects directory must not be followed"
        );
        Ok(())
    }
}
