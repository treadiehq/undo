use anyhow::Result;
use similar::{ChangeTag, TextDiff};
use std::io::Read;
use std::path::Path;

use crate::db::Database;
use crate::models::FileEvent;
use crate::snapshots;
use crate::{BOLD, DIM, GREEN, RED, RESET, find_project};

/// Heuristic: treat content as binary if it contains a NUL byte within the
/// first 8 KiB (same approach used by git and most editors).
pub(crate) fn is_binary(data: &[u8]) -> bool {
    data.iter().take(8192).any(|&b| b == 0)
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
    let project = find_project(&db, &cwd)?;

    let abs_path = crate::safe_resolve_path(&cwd, path_str, &project.root_path)?;
    let abs_path_str = abs_path.to_string_lossy().to_string();

    let event = match diff_event(&db, project.id, &abs_path_str, duration, checkpoint)? {
        Some(e) => e,
        None => {
            println!("No saved version matches this selection.");
            return Ok(());
        }
    };

    let hash = match saved_hash_for_diff(&event) {
        Some(h) => h,
        None => {
            println!("No saved version matches this selection.");
            return Ok(());
        }
    };

    let snapshot_content = snapshots::load(project.id, hash)?;

    if is_binary(&snapshot_content) {
        println!("The selected saved version is binary, so a text comparison is not available.");
        return Ok(());
    }

    let snapshot_text = String::from_utf8_lossy(&snapshot_content);

    if !abs_path.exists() {
        if event.event_type == "DELETED" {
            println!(
                "This file is deleted. Recover it with:\n  {}undo restore-deleted {:?}{}",
                BOLD, path_str, RESET
            );
            return Ok(());
        }

        println!("The current file is missing. Showing the selected saved version.");
        println!();
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

    let current_text = String::from_utf8_lossy(&current_content);

    if snapshot_text == current_text {
        println!("The current file matches the selected saved version.");
        return Ok(());
    }

    let rel = crate::relative_path(&abs_path_str, &project.root_path);
    if summary || stat {
        print_diff_stats(&snapshot_text, &current_text, rel);
    }
    if !summary {
        print_unified_diff(&snapshot_text, &current_text, rel);
    }

    Ok(())
}

fn diff_event(
    db: &Database,
    project_id: i64,
    path: &str,
    duration: Option<&str>,
    checkpoint: Option<&str>,
) -> Result<Option<FileEvent>> {
    match (duration, checkpoint) {
        (Some(_), Some(_)) => anyhow::bail!("use either a duration or --checkpoint, not both"),
        (Some(duration), None) => {
            let secs = crate::duration::parse_duration(duration)?;
            let target_time = chrono::Utc::now().timestamp().saturating_sub(secs);
            db.get_event_at_time(project_id, path, target_time)
        }
        (None, Some(name)) => {
            let checkpoint = db
                .get_checkpoint_by_ref(project_id, name)?
                .ok_or_else(|| anyhow::anyhow!("checkpoint '{}' not found", name))?;
            db.get_event_at_time(project_id, path, checkpoint.timestamp)
        }
        (None, None) => db.get_latest_event(project_id, path),
    }
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
    let old_text = String::from_utf8_lossy(old);
    let new_text = String::from_utf8_lossy(new);
    if old_text == new_text {
        println!("{}: no content changes.", path);
        return Ok(());
    }
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
}
