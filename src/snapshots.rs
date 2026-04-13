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
