use anyhow::Result;
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub const MAX_SNAPSHOT_SIZE: usize = 100 * 1024 * 1024; // 100 MiB

pub fn hash_bytes(content: &[u8]) -> String {
    crate::to_hex(&Sha256::digest(content))
}

/// Process-wide counter giving each in-flight temp file a unique name, so the
/// parallel initial scan can have several workers writing distinct snapshots
/// (or even the same hash) at once without colliding on a shared `.gz.tmp` path.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn snapshot_dir_in(base: &Path, project_id: i64) -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = base.join("snapshots").join(project_id.to_string());
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    Ok(dir)
}

fn snapshot_dir(project_id: i64) -> Result<PathBuf> {
    snapshot_dir_in(&crate::backtrack_dir()?, project_id)
}

/// Full filesystem path for a snapshot identified by content hash.
pub fn snapshot_path(project_id: i64, hash: &str) -> Result<PathBuf> {
    Ok(snapshot_dir(project_id)?.join(format!("{}.gz", hash)))
}

/// Compress and store file content under an explicit data-dir `base`. Returns the
/// path string for DB storage. Deduplicates automatically — if a snapshot with the
/// same hash exists, skips the write.
///
/// When `durable` is true the snapshot's bytes are fsync'd and the parent directory
/// is fsync'd before returning, so both the content and the rename that publishes it
/// survive power loss. The live watch path needs this because a snapshot can be the
/// only surviving copy of content that has since been overwritten or deleted. The
/// initial scan passes `durable = false`: its snapshots are regenerable from the
/// still-present source file, so an fsync per file (up to `MAX_FILES`) would be a
/// large, pointless throughput regression.
///
/// Taking `base` explicitly (rather than resolving `backtrack_dir()` internally)
/// lets the parallel initial scan call this from worker threads, which do not
/// inherit the test data-dir thread-local override.
fn write_snapshot_in(
    base: &Path,
    project_id: i64,
    hash: &str,
    content: &[u8],
    durable: bool,
) -> Result<String> {
    let dir = snapshot_dir_in(base, project_id)?;
    let path = dir.join(format!("{}.gz", hash));
    if !path.exists() {
        // Write to a uniquely-named temp file then rename atomically. POSIX
        // guarantees rename is atomic on the same filesystem, so a crash mid-write
        // can never leave a partial file at the final path that would be mistaken
        // for a valid snapshot. The unique suffix keeps concurrent writers of the
        // same hash from racing on a shared temp path.
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = path.with_file_name(format!("{}.gz.tmp.{}.{}", hash, std::process::id(), seq));
        let _ = fs::remove_file(&tmp); // remove stale temp if present
        let write_result = (|| -> Result<()> {
            use std::os::unix::fs::OpenOptionsExt;
            let file = fs::OpenOptions::new()
                .write(true)
                .create_new(true) // O_CREAT | O_EXCL — refuses to follow symlinks
                .mode(0o600)
                .open(&tmp)?;
            let mut encoder = GzEncoder::new(file, Compression::fast());
            encoder.write_all(content)?;
            let file = encoder.finish()?;
            if durable {
                // Flush the snapshot's bytes before the rename publishes it, so a
                // committed `file_events` row can never reference a snapshot whose
                // contents never reached disk.
                file.sync_all()?;
            }
            fs::rename(&tmp, &path)?;
            if durable {
                // Persist the directory entry created by the rename; without this
                // the rename can be lost to power loss even though the file data is
                // already durable.
                fsync_dir(&dir)?;
            }
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&tmp); // clean up any partial temp file
        }
        write_result?;
    }
    Ok(path.to_string_lossy().to_string())
}

/// Fast, non-durable snapshot write under an explicit data-dir `base`, used by the
/// parallel initial scan (see [`write_snapshot_in`] for why the scan skips fsync).
pub fn save_in(base: &Path, project_id: i64, hash: &str, content: &[u8]) -> Result<String> {
    write_snapshot_in(base, project_id, hash, content, false)
}

/// Durable snapshot write for the live watch path: fsyncs the snapshot file and its
/// parent directory so a freshly captured version survives power loss. Use this for
/// content that may be irreplaceable; the scan path uses the non-durable [`save_in`].
pub fn save_durable(project_id: i64, hash: &str, content: &[u8]) -> Result<String> {
    write_snapshot_in(&crate::backtrack_dir()?, project_id, hash, content, true)
}

/// fsync a directory so a rename into it survives power loss. Best-effort across
/// platforms: on Linux this persists the new dirent; on macOS plain `fsync` is
/// weaker than `F_FULLFSYNC` but still flushes to the device cache (we accept that
/// ceiling rather than pay `F_FULLFSYNC` on every write).
pub(crate) fn fsync_dir(dir: &Path) -> std::io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

/// Load and decompress a snapshot, returning the original file content.
/// Caps decompressed output at `MAX_SNAPSHOT_SIZE` to prevent gzip bombs.
pub fn load(project_id: i64, hash: &str) -> Result<Vec<u8>> {
    let path = snapshot_path(project_id, hash)?;
    let file = fs::File::open(&path).map_err(|e| {
        anyhow::anyhow!(
            "Saved version is missing: snapshot not found ({}): {}",
            hash,
            e
        )
    })?;
    let decoder = GzDecoder::new(file);
    let mut content = Vec::with_capacity(8192);
    let limit = MAX_SNAPSHOT_SIZE as u64 + 1;
    let n = decoder.take(limit).read_to_end(&mut content).map_err(|e| {
        anyhow::anyhow!(
            "Saved version is unreadable: could not decompress snapshot {}: {}",
            hash,
            e
        )
    })?;
    if n as u64 >= limit {
        anyhow::bail!(
            "Saved version is unreadable: snapshot {} decompresses beyond the {}-byte limit",
            hash,
            MAX_SNAPSHOT_SIZE,
        );
    }
    Ok(content)
}

/// Count snapshot files on disk for a project.
///
/// Counts only `*.gz` files — a published snapshot is always named `<hash>.gz`.
/// In-flight or leaked temp writes are named `<hash>.gz.tmp.<pid>.<seq>` (so
/// their extension is the sequence number, not `gz`); counting those would
/// inflate the `Saved:` figure in `undo status` after an interrupted write.
pub fn count(project_id: i64) -> Result<usize> {
    let dir = crate::backtrack_dir()?
        .join("snapshots")
        .join(project_id.to_string());
    if !dir.exists() {
        return Ok(0);
    }
    Ok(fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("gz"))
        .count())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Content saved to a snapshot is recovered byte-for-byte on load.
    #[test]
    fn save_and_load_round_trip() {
        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());

        let content = b"hello, snapshot world\n";
        save_durable(1, "roundtrip_hash", content).unwrap();
        let loaded = load(1, "roundtrip_hash").unwrap();
        assert_eq!(loaded, content);
    }

    /// Saving the same hash twice must not corrupt the snapshot or create duplicate files.
    #[test]
    fn save_is_idempotent_for_same_hash() {
        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());

        let content = b"duplicate content to save twice";
        save_durable(1, "dedup_hash", content).unwrap();
        // Second call must succeed — path.exists() guard skips the write.
        save_durable(1, "dedup_hash", content).unwrap();
        let loaded = load(1, "dedup_hash").unwrap();
        assert_eq!(loaded, content);
    }

    /// Loading a hash with no backing file returns a clear error rather than panicking.
    #[test]
    fn load_nonexistent_hash_returns_error() {
        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());

        let result = load(1, "this_hash_does_not_exist_xyz");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.starts_with("Saved version is missing"), "got: {}", msg);
        assert!(msg.contains("snapshot not found"), "got: {}", msg);
    }

    /// A snapshot that cannot be decompressed names the user-facing problem first
    /// and keeps the hash and decompression error as diagnostics.
    #[test]
    fn load_corrupt_snapshot_leads_with_unreadable_version() {
        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());

        let path = snapshot_path(1, "corrupt_hash").unwrap();
        fs::write(path, b"not a gzip stream").unwrap();

        let msg = load(1, "corrupt_hash").unwrap_err().to_string();
        assert!(
            msg.starts_with("Saved version is unreadable"),
            "got: {}",
            msg
        );
        assert!(msg.contains("corrupt_hash"), "got: {}", msg);
        assert!(msg.contains("decompress"), "got: {}", msg);
    }

    /// count() reflects the number of distinct saved snapshots and is unaffected by deduplication.
    #[test]
    fn count_returns_correct_number_of_snapshots() {
        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());

        assert_eq!(count(42).unwrap(), 0, "no snapshots yet");
        save_durable(42, "hash_a", b"content a").unwrap();
        assert_eq!(count(42).unwrap(), 1);
        save_durable(42, "hash_b", b"content b").unwrap();
        assert_eq!(count(42).unwrap(), 2);
        // Saving the same hash again must not increase the count (deduplication).
        save_durable(42, "hash_a", b"content a").unwrap();
        assert_eq!(count(42).unwrap(), 2);
    }

    /// A leaked temp file from an interrupted durable write (`<hash>.gz.tmp.<pid>.<seq>`)
    /// must NOT be counted as a snapshot — its extension is the sequence number,
    /// not `gz`. Before the `.gz`-only filter, `count()` tallied every dir entry,
    /// so a power-loss-leaked temp inflated the `Saved:` figure in `undo status`.
    #[test]
    fn count_ignores_leaked_temp_files() {
        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());

        save_durable(7, "realhash", b"real snapshot").unwrap();
        assert_eq!(count(7).unwrap(), 1);

        // Simulate a temp file left behind by an interrupted/killed write.
        let dir = crate::backtrack_dir().unwrap().join("snapshots").join("7");
        let leaked = dir.join(format!("realhash.gz.tmp.{}.0", std::process::id()));
        fs::write(&leaked, b"half-written gzip").unwrap();

        assert_eq!(
            count(7).unwrap(),
            1,
            "a leaked .gz.tmp temp file must not be counted as a snapshot"
        );
    }
}
