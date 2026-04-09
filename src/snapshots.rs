use anyhow::Result;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;

pub const MAX_SNAPSHOT_SIZE: usize = 100 * 1024 * 1024; // 100 MB

fn snapshot_dir(project_id: i64) -> Result<PathBuf> {
    let dir = crate::backtrack_dir()?
        .join("snapshots")
        .join(project_id.to_string());
    fs::create_dir_all(&dir)?;
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
        let file = fs::File::create(&path)?;
        let mut encoder = GzEncoder::new(file, Compression::fast());
        encoder.write_all(content)?;
        encoder.finish()?;
    }
    Ok(path.to_string_lossy().to_string())
}

/// Load and decompress a snapshot, returning the original file content.
pub fn load(project_id: i64, hash: &str) -> Result<Vec<u8>> {
    let path = snapshot_path(project_id, hash)?;
    let file = fs::File::open(&path)
        .map_err(|e| anyhow::anyhow!("snapshot not found ({}): {}", hash, e))?;
    let mut decoder = GzDecoder::new(file);
    let mut content = Vec::new();
    decoder.read_to_end(&mut content)?;
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
