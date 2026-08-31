use anyhow::Result;
use similar::{ChangeTag, TextDiff};
use std::borrow::Cow;
use std::fmt::Write as _;
use std::io::Read;
use std::path::Path;

use crate::db::Database;
use crate::models::FileEvent;
use crate::restore::{self, RestoreKind};
use crate::snapshots;
use crate::{BOLD, DIM, GREEN, RED, RESET, resolve_project};

#[derive(Clone, Debug, Eq, PartialEq)]
struct DiffSelection {
    hash: String,
    deleted_fallback: bool,
}

/// Heuristic: treat content as binary if it contains a NUL byte within the
/// first 8 KiB (same approach used by git and most editors).
pub(crate) fn is_binary(data: &[u8]) -> bool {
    data.iter().take(8192).any(|&b| b == 0)
}

/// Render a pair of byte strings for an exact text diff. Valid UTF-8 remains
/// unchanged. If either side is invalid UTF-8, both sides use an injective byte
/// representation so distinct bytes can never collapse to the same U+FFFD text.
pub(crate) fn render_bytes_for_diff<'a>(
    old: &'a [u8],
    new: &'a [u8],
) -> (Cow<'a, str>, Cow<'a, str>) {
    match (std::str::from_utf8(old), std::str::from_utf8(new)) {
        (Ok(old), Ok(new)) => (Cow::Borrowed(old), Cow::Borrowed(new)),
        _ => (Cow::Owned(escape_bytes(old)), Cow::Owned(escape_bytes(new))),
    }
}

fn render_bytes(data: &[u8]) -> Cow<'_, str> {
    std::str::from_utf8(data)
        .map(Cow::Borrowed)
        .unwrap_or_else(|_| Cow::Owned(escape_bytes(data)))
}

fn escape_bytes(data: &[u8]) -> String {
    let mut rendered = String::with_capacity(data.len());
    for &byte in data {
        match byte {
            b'\n' => rendered.push('\n'),
            b' '..=b'~' if byte != b'\\' => rendered.push(char::from(byte)),
            b'\\' => rendered.push_str("\\\\"),
            b'\t' => rendered.push_str("\\t"),
            b'\r' => rendered.push_str("\\r"),
            _ => write!(&mut rendered, "\\x{byte:02x}")
                .expect("writing escaped bytes to a string cannot fail"),
        }
    }
    rendered
}

/// Read at most `limit` bytes from `path`. Returns `None` if the file is
/// larger than the limit so callers can degrade gracefully instead of OOM-ing.
pub(crate) fn read_capped(path: &Path, limit: usize) -> Result<Option<Vec<u8>>> {
    let file = std::fs::File::open(path)?;
    let cap = limit as u64 + 1;
    let mut buf = Vec::new();
    let n = file.take(cap).read_to_end(&mut buf)?;
    if n as u64 >= cap {
        return Ok(None);
    }
    Ok(Some(buf))
}

pub fn cmd_diff(
    path_str: &str,
    duration: Option<&str>,
    checkpoint: Option<&str>,
    summary: bool,
    stat: bool,
) -> Result<()> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = resolve_project(&db, &cwd)?;

    let abs_path = crate::safe_resolve_path(&cwd, path_str, &project.root_path)?;
    let abs_path_str = abs_path.to_string_lossy().to_string();

    let selection = match diff_selection(&db, project.id, &abs_path_str, duration, checkpoint)? {
        Some(selection) => selection,
        None => {
            println!("No saved version matches this selection.");
            return Ok(());
        }
    };

    let snapshot_content = snapshots::load(project.id, &selection.hash)?;

    if is_binary(&snapshot_content) {
        println!("The selected saved version is binary, so a text comparison is not available.");
        return Ok(());
    }

    if !abs_path.exists() {
        if selection.deleted_fallback {
            println!(
                "This file is deleted. Recover it with:\n  {}undo restore-deleted {:?}{}",
                BOLD, path_str, RESET
            );
            return Ok(());
        }

        println!("The current file is missing. Showing the selected saved version.");
        println!();
        let snapshot_text = render_bytes(&snapshot_content);
        for line in snapshot_text.lines() {
            println!(" {}", line);
        }
        return Ok(());
    }

    let current_content = match read_capped(&abs_path, snapshots::MAX_SNAPSHOT_SIZE)? {
        Some(c) => c,
        None => {
            println!(
                "Current file is larger than {} bytes, so Undo will not compare it.",
                snapshots::MAX_SNAPSHOT_SIZE
            );
            return Ok(());
        }
    };

    if is_binary(&current_content) {
        println!("The current file is binary, so a text comparison is not available.");
        return Ok(());
    }

    if snapshot_content == current_content {
        println!("The current file matches the selected saved version.");
        return Ok(());
    }

    let (snapshot_text, current_text) = render_bytes_for_diff(&snapshot_content, &current_content);
    let rel = crate::relative_path(&abs_path_str, &project.root_path);
    if summary || stat {
        print_diff_stats(&snapshot_text, &current_text, rel);
    }
    if !summary {
        print_unified_diff(&snapshot_text, &current_text, rel);
    }

    Ok(())
}

fn diff_selection(
    db: &Database,
    project_id: i64,
    path: &str,
    duration: Option<&str>,
    checkpoint: Option<&str>,
) -> Result<Option<DiffSelection>> {
    match (duration, checkpoint) {
        (Some(_), Some(_)) => anyhow::bail!("use either a duration or --checkpoint, not both"),
        (Some(duration), None) => {
            let secs = crate::duration::parse_duration(duration)?;
            let target_time = chrono::Utc::now().timestamp().saturating_sub(secs);
            diff_selection_at_time(db, project_id, path, target_time)
        }
        (None, Some(name)) => {
            let checkpoint = db
                .get_checkpoint_by_ref(project_id, name)?
                .ok_or_else(|| anyhow::anyhow!("checkpoint '{}' not found", name))?;
            diff_selection_at_time(db, project_id, path, checkpoint.timestamp)
        }
        (None, None) => Ok(db
            .get_latest_event(project_id, path)?
            .and_then(diff_selection_from_event)),
    }
}

fn diff_selection_at_time(
    db: &Database,
    project_id: i64,
    path: &str,
    target_time: i64,
) -> Result<Option<DiffSelection>> {
    Ok(
        restore::resolve_restore_source(db, project_id, path, target_time)?.map(|source| {
            DiffSelection {
                hash: source.hash,
                deleted_fallback: source.kind == RestoreKind::DeletedFallback,
            }
        }),
    )
}

fn diff_selection_from_event(event: FileEvent) -> Option<DiffSelection> {
    let deleted_fallback = event.event_type == "DELETED";
    let hash = saved_hash_for_diff(&event)?.to_string();
    Some(DiffSelection {
        hash,
        deleted_fallback,
    })
}

pub(crate) fn saved_hash_for_diff(event: &FileEvent) -> Option<&str> {
    if event.event_type == "DELETED" {
        event.previous_hash.as_deref()
    } else {
        event.current_hash.as_deref()
    }
}

pub(crate) fn print_unified_diff(old: &str, new: &str, path: &str) {
    print_unified_diff_with_labels(old, new, path, "selected saved", "current");
}

pub(crate) fn print_unified_diff_with_labels(
    old: &str,
    new: &str,
    path: &str,
    old_label: &str,
    new_label: &str,
) {
    let diff = TextDiff::from_lines(old, new);

    println!("{}--- {:<9}{}{}", DIM, old_label, path, RESET);
    println!("{}+++ {:<9}{}{}", DIM, new_label, path, RESET);
    println!();

    for (idx, group) in diff.grouped_ops(3).iter().enumerate() {
        if idx > 0 {
            println!("{}…{}", DIM, RESET);
        }
        for op in group {
            for change in diff.iter_changes(op) {
                match change.tag() {
                    ChangeTag::Delete => {
                        print!("{}-{}{}", RED, change, RESET);
                    }
                    ChangeTag::Insert => {
                        print!("{}+{}{}", GREEN, change, RESET);
                    }
                    ChangeTag::Equal => {
                        print!(" {}", change);
                    }
                }
                if change.missing_newline() {
                    println!();
                }
            }
        }
    }
}

pub(crate) fn print_diff_stats(old: &str, new: &str, path: &str) {
    let diff = TextDiff::from_lines(old, new);
    let mut inserted = 0usize;
    let mut deleted = 0usize;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Delete => deleted += 1,
            ChangeTag::Insert => inserted += 1,
            ChangeTag::Equal => {}
        }
    }
    println!("{} | +{} -{}", path, inserted, deleted);
}

pub(crate) fn print_bytes_diff(
    old: &[u8],
    new: &[u8],
    path: &str,
    old_label: &str,
    new_label: &str,
) -> Result<()> {
    if is_binary(old) || is_binary(new) {
        println!("{}: binary file — text comparison not available.", path);
        return Ok(());
    }
    if old == new {
        println!("{}: no content changes.", path);
        return Ok(());
    }
    let (old_text, new_text) = render_bytes_for_diff(old, new);
    print_unified_diff_with_labels(&old_text, &new_text, path, old_label, new_label);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(
        event_type: &str,
        current_hash: Option<&str>,
        previous_hash: Option<&str>,
    ) -> FileEvent {
        FileEvent {
            id: 1,
            project_id: 1,
            timestamp: 0,
            path: "/project/file.txt".to_string(),
            event_type: event_type.to_string(),
            current_hash: current_hash.map(str::to_string),
            previous_hash: previous_hash.map(str::to_string),
            snapshot_path: None,
            old_path: None,
            file_size: None,
        }
    }

    /// A NUL byte within the first 8 KiB marks content as binary.
    #[test]
    fn is_binary_detects_nul_byte() {
        assert!(is_binary(b"hello\x00world"));
    }

    /// Ordinary source text with no NUL bytes is not binary.
    #[test]
    fn is_binary_returns_false_for_plain_text() {
        assert!(!is_binary(b"fn main() {\n    println!(\"hello\");\n}\n"));
    }

    /// A NUL at position 8192 is outside the inspection window and must not trigger the binary flag.
    #[test]
    fn is_binary_ignores_nul_beyond_8192_bytes() {
        // A NUL at position 8192 (0-indexed) is outside the 8 KiB inspection
        // window, so the content should be treated as text.
        let mut data = vec![b'a'; 8193];
        data[8192] = 0;
        assert!(!is_binary(&data));
    }

    #[test]
    fn invalid_utf8_bytes_render_as_distinct_lossless_text() {
        let old = b"\xe9l\xe8ve";
        let new = b"\xe9l\xe9ve";
        assert_ne!(old, new);

        let (old_text, new_text) = render_bytes_for_diff(old, new);

        assert_eq!(old_text, "\\xe9l\\xe8ve");
        assert_eq!(new_text, "\\xe9l\\xe9ve");
        assert_ne!(old_text, new_text);
    }

    #[test]
    fn escaped_invalid_bytes_do_not_collide_with_literal_escape_text() {
        let invalid = b"\xff";
        let literal = b"\\xff";

        let (invalid_text, literal_text) = render_bytes_for_diff(invalid, literal);

        assert_eq!(invalid_text, "\\xff");
        assert_eq!(literal_text, "\\\\xff");
        assert_ne!(invalid_text, literal_text);
    }

    #[test]
    fn valid_utf8_diff_rendering_is_unchanged() {
        let (old, new) = render_bytes_for_diff("élève".as_bytes(), "élève!".as_bytes());
        assert_eq!(old, "élève");
        assert_eq!(new, "élève!");
    }

    /// A file under the byte limit is read in full and returned as Some.
    #[test]
    fn read_capped_returns_content_under_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.txt");
        std::fs::write(&path, b"hello").unwrap();
        let got = read_capped(&path, 100).unwrap();
        assert_eq!(got, Some(b"hello".to_vec()));
    }

    /// A file larger than the limit must return None rather than allocating
    /// the whole file — this is the OOM guard for `cmd_diff`.
    #[test]
    fn read_capped_returns_none_when_file_exceeds_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.txt");
        std::fs::write(&path, vec![b'x'; 1024]).unwrap();
        let got = read_capped(&path, 100).unwrap();
        assert_eq!(got, None, "files over the cap must not be loaded");
    }

    /// A file at exactly the limit is still readable — boundary check.
    #[test]
    fn read_capped_returns_content_at_exact_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("exact.txt");
        std::fs::write(&path, vec![b'y'; 100]).unwrap();
        let got = read_capped(&path, 100).unwrap();
        assert_eq!(got, Some(vec![b'y'; 100]));
    }

    /// A deleted file's last known contents live in `previous_hash`. This is
    /// what lets `undo diff` compare that content against a file recreated on
    /// disk after the daemon recorded the deletion.
    #[test]
    fn deleted_event_uses_previous_hash_for_diff() {
        let e = event("DELETED", None, Some("last_live_hash"));
        assert_eq!(saved_hash_for_diff(&e), Some("last_live_hash"));
    }

    /// Normal events still diff from their current snapshot.
    #[test]
    fn non_deleted_event_uses_current_hash_for_diff() {
        let e = event("MODIFIED", Some("current_hash"), Some("older_hash"));
        assert_eq!(saved_hash_for_diff(&e), Some("current_hash"));
    }

    /// If a malformed deleted event has no previous hash, diff should report
    /// that no saved version is available instead of guessing.
    #[test]
    fn deleted_event_without_previous_hash_has_no_saved_diff_source() {
        let e = event("DELETED", None, None);
        assert_eq!(saved_hash_for_diff(&e), None);
    }

    fn deleted_only_history() -> (Database, i64, String) {
        let db = Database::open_in_memory().unwrap();
        let project = db.get_or_create_project(Path::new("/project")).unwrap();
        let path = "/project/deleted.txt".to_string();
        db.insert_event(
            project.id,
            &path,
            "DELETED",
            None,
            Some("last_live_hash"),
            None,
            None,
            Some(10),
        )
        .unwrap();
        (db, project.id, path)
    }

    /// When retention leaves only the DELETED row, duration and checkpoint
    /// queries must still resolve its pinned previous_hash (#92).
    #[test]
    fn time_based_diff_recovers_deleted_only_history() {
        let (db, project_id, path) = deleted_only_history();
        db.create_checkpoint_now(project_id, None, "after-delete", None)
            .unwrap();

        for (selection, deleted_fallback) in [
            (
                diff_selection(&db, project_id, &path, Some("1s"), None).unwrap(),
                false,
            ),
            (
                diff_selection(&db, project_id, &path, None, Some("after-delete")).unwrap(),
                true,
            ),
        ] {
            assert_eq!(
                selection,
                Some(DiffSelection {
                    hash: "last_live_hash".to_string(),
                    deleted_fallback,
                })
            );
        }
    }

    /// A deletion after the selected boundary carries the exact pre-deletion
    /// content in previous_hash, even if every older event was pruned.
    #[test]
    fn diff_uses_first_later_deletion_to_reconstruct_pruned_boundary() {
        let (db, project_id, path) = deleted_only_history();
        let selection = diff_selection_at_time(&db, project_id, &path, 0)
            .unwrap()
            .unwrap();

        assert_eq!(selection.hash, "last_live_hash");
        assert!(
            !selection.deleted_fallback,
            "content immediately before a later deletion is an exact boundary source"
        );
    }

    /// Rename-away events describe absence after the rename and the old path's
    /// previous content before it. Reusing restore's resolver preserves both
    /// sides without an ad-hoc old_path filter in diff.
    #[test]
    fn time_based_diff_preserves_rename_boundaries() {
        let db = Database::open_in_memory().unwrap();
        let project = db.get_or_create_project(Path::new("/project")).unwrap();
        let old_path = "/project/old.txt";
        db.insert_event(
            project.id,
            "/project/new.txt",
            "RENAMED",
            Some("renamed_hash"),
            Some("old_path_hash"),
            None,
            Some(old_path),
            Some(10),
        )
        .unwrap();

        assert!(
            diff_selection_at_time(&db, project.id, old_path, chrono::Utc::now().timestamp())
                .unwrap()
                .is_none(),
            "the old path is absent after rename-away"
        );
        assert_eq!(
            diff_selection_at_time(&db, project.id, old_path, 0)
                .unwrap()
                .map(|selection| selection.hash),
            Some("old_path_hash".to_string()),
            "the old path existed immediately before the rename"
        );
    }
}
