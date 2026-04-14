use anyhow::Result;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;

pub const MAX_SNAPSHOT_SIZE: usize = 100 * 1024 * 1024; // 100 MB

fn snapshot_dir(project_id: i64) -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = crate::backtrack_dir()?
        .join("snapshots")
        .join(project_id.to_string());
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    Ok(dir)
}

/// Full filesystem path for a snapshot identified by content hash.
pub fn snapshot_path(project_id: i64, hash: &str) -> Result<PathBuf> {
    Ok(snapshot_dir(project_id)?.join(format!("{}.gz", hash)))
}

/// Compress and store file content. Returns the path string for DB storage.
/// Deduplicates automatically — if a snapshot with the same hash exists, skips the write.
pub fn save(project_id: i64, hash: &str, content: &[u8]) -> Result<String> {
    let path = snapshot_path(project_id, hash)?;
    if !path.exists() {
        // Write to a temp file then rename atomically. POSIX guarantees rename
        // is atomic on the same filesystem, so a crash mid-write can never
        // leave a partial file at the final path that would be mistaken for a
        // valid snapshot on the next run.
        let tmp = path.with_extension("gz.tmp");
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
            encoder.finish()?;
            fs::rename(&tmp, &path)?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&tmp); // clean up any partial temp file
        }
        write_result?;
    }
    Ok(path.to_string_lossy().to_string())
}

/// Load and decompress a snapshot, returning the original file content.
/// Caps decompressed output at `MAX_SNAPSHOT_SIZE` to prevent gzip bombs.
pub fn load(project_id: i64, hash: &str) -> Result<Vec<u8>> {
    let path = snapshot_path(project_id, hash)?;
    let file = fs::File::open(&path)
        .map_err(|e| anyhow::anyhow!("snapshot not found ({}): {}", hash, e))?;
    let decoder = GzDecoder::new(file);
    let mut content = Vec::with_capacity(8192);
    let limit = MAX_SNAPSHOT_SIZE as u64 + 1;
    let n = decoder.take(limit).read_to_end(&mut content)?;
    if n as u64 >= limit {
        anyhow::bail!(
            "snapshot {} decompresses beyond {} limit — refusing to load",
            hash,
            MAX_SNAPSHOT_SIZE,
        );
    }
    Ok(content)
}

/// Count snapshot files on disk for a project.
pub fn count(project_id: i64) -> Result<usize> {
    let dir = crate::backtrack_dir()?
        .join("snapshots")
        .join(project_id.to_string());
    if !dir.exists() {
        return Ok(0);
    }
    Ok(fs::read_dir(dir)?.filter_map(|e| e.ok()).count())
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
        save(1, "roundtrip_hash", content).unwrap();
        let loaded = load(1, "roundtrip_hash").unwrap();
        assert_eq!(loaded, content);
    }

    /// Saving the same hash twice must not corrupt the snapshot or create duplicate files.
    #[test]
    fn save_is_idempotent_for_same_hash() {
        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());

        let content = b"duplicate content to save twice";
        save(1, "dedup_hash", content).unwrap();
        // Second call must succeed — path.exists() guard skips the write.
        save(1, "dedup_hash", content).unwrap();
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
        assert!(msg.contains("snapshot not found"), "got: {}", msg);
    }

    /// count() reflects the number of distinct saved snapshots and is unaffected by deduplication.
    #[test]
    fn count_returns_correct_number_of_snapshots() {
        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());

        assert_eq!(count(42).unwrap(), 0, "no snapshots yet");
        save(42, "hash_a", b"content a").unwrap();
        assert_eq!(count(42).unwrap(), 1);
        save(42, "hash_b", b"content b").unwrap();
        assert_eq!(count(42).unwrap(), 2);
        // Saving the same hash again must not increase the count (deduplication).
        save(42, "hash_a", b"content a").unwrap();
        assert_eq!(count(42).unwrap(), 2);
    }
}
