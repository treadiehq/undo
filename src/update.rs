use anyhow::{Context, Result};
use std::process::Command;

const REPO: &str = "treadiehq/undo";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn cmd_update() -> Result<()> {
    println!("{}undo{} — self-update", crate::BOLD, crate::RESET);
    println!();
    println!("  Current version: v{}", CURRENT_VERSION);

    let latest_tag = fetch_latest_tag()?;
    let latest_ver = latest_tag.strip_prefix('v').unwrap_or(&latest_tag);

    println!("  Latest release:  {}", latest_tag);
    println!();

    if latest_ver == CURRENT_VERSION {
        println!("Already up to date.");
        return Ok(());
    }

    let target = detect_target()?;
    let url = format!(
        "https://github.com/{REPO}/releases/download/{latest_tag}/undo-{latest_tag}-{target}.tar.gz"
    );

    println!("Downloading undo {} for {}...", latest_tag, target);

    let tmpdir = tempfile::TempDir::new().context("failed to create temp directory")?;

    let tarball = tmpdir.path().join("undo.tar.gz");

    let dl_status = Command::new("curl")
        .args(["-fsSL", &url, "-o"])
        .arg(&tarball)
        .status()
        .context("failed to run curl — is it installed?")?;

    if !dl_status.success() {
        anyhow::bail!(
            "download failed (HTTP error). Check that release {} exists for {}.",
            latest_tag,
            target
        );
    }

    let tar_status = Command::new("tar")
        .args(["xzf"])
        .arg(&tarball)
        .arg("-C")
        .arg(tmpdir.path())
        .status()
        .context("failed to extract archive")?;

    if !tar_status.success() {
        anyhow::bail!("failed to extract downloaded archive");
    }

    let new_binary = tmpdir.path().join("undo");
    if !new_binary.exists() {
        anyhow::bail!("extracted archive does not contain 'undo' binary");
    }

    let current_exe =
        std::env::current_exe().context("cannot determine current executable path")?;

    // Move the old binary aside, then install the new one. Both steps must work
    // even though `new_binary` lives in a tempdir that is very often on a
    // DIFFERENT filesystem than the install location (e.g. `/tmp` is tmpfs on
    // most Linux distros; `$TMPDIR` is a separate APFS volume on macOS). A bare
    // `rename` across filesystems fails with EXDEV, which silently broke every
    // self-update in those (very common) setups.
    let backup = current_exe.with_extension("old");
    std::fs::rename(&current_exe, &backup)
        .context("failed to replace binary — try running with sudo")?;

    if let Err(e) = install_binary(&new_binary, &current_exe) {
        std::fs::rename(&backup, &current_exe).ok();
        return Err(e).context("failed to install new binary");
    }

    std::fs::remove_file(&backup).ok();
    // tmpdir auto-cleans on drop

    println!(
        "\n{}Updated{} undo from v{} to {}.",
        crate::GREEN,
        crate::RESET,
        CURRENT_VERSION,
        latest_tag
    );

    Ok(())
}

fn fetch_latest_tag() -> Result<String> {
    let output = Command::new("curl")
        .args([
            "-fsSL",
            &format!("https://api.github.com/repos/{REPO}/releases/latest"),
        ])
        .output()
        .context("failed to run curl — is it installed?")?;

    if !output.status.success() {
        anyhow::bail!("failed to fetch latest release from GitHub");
    }

    let body = String::from_utf8_lossy(&output.stdout);

    // Minimal JSON parsing to avoid adding serde_json as a dependency.
    // Looks for "tag_name": "v0.1.2"
    let tag = body
        .split("\"tag_name\"")
        .nth(1)
        .and_then(|s| s.split('"').nth(1))
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("could not parse latest release tag from GitHub API"))?;

    Ok(tag)
}

/// Install `src` to `dest`, working across filesystems.
///
/// `src` usually lives in a tempdir on a different filesystem than `dest`, so a
/// direct `rename(src, dest)` fails with EXDEV. Instead, copy `src` to a temp
/// file that is a SIBLING of `dest` (guaranteed same filesystem) and then
/// `rename` it into place — the rename is atomic, so a crash mid-install never
/// leaves a half-written executable at `dest`. The executable bit is set before
/// the rename so `dest` is runnable the instant it appears.
fn install_binary(src: &std::path::Path, dest: &std::path::Path) -> Result<()> {
    let staged = staged_path(dest);
    let _ = std::fs::remove_file(&staged);

    let result = (|| -> std::io::Result<()> {
        std::fs::copy(src, &staged)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
        }
        std::fs::rename(&staged, dest)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    result.context("failed to stage and install new binary")
}

/// A temp path adjacent to `dest` (same directory, hence same filesystem) so
/// the final `rename` into `dest` is an intra-filesystem atomic move.
fn staged_path(dest: &std::path::Path) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".undo-update.{}.{}", std::process::id(), nanos));
    match dest.parent() {
        Some(parent) => parent.join(name),
        None => std::path::PathBuf::from(name),
    }
}

fn detect_target() -> Result<String> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;

    let os_part = match os {
        "macos" => "apple-darwin",
        "linux" => "unknown-linux-gnu",
        _ => anyhow::bail!("unsupported OS: {}", os),
    };

    let arch_part = match arch {
        "aarch64" => "aarch64",
        "x86_64" => "x86_64",
        _ => anyhow::bail!("unsupported architecture: {}", arch),
    };

    Ok(format!("{}-{}", arch_part, os_part))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// The staged temp file must live in the SAME directory as the destination
    /// so the final rename is intra-filesystem (never EXDEV). The previous code
    /// renamed straight from the tempdir, which crosses filesystems whenever
    /// `$TMPDIR`/`/tmp` differs from the install dir.
    #[test]
    fn staged_path_is_sibling_of_destination() {
        let dest = Path::new("/usr/local/bin/undo");
        let staged = staged_path(dest);
        assert_eq!(
            staged.parent(),
            dest.parent(),
            "staged file must share the destination's directory"
        );
        assert_ne!(staged, dest, "staged path must differ from the destination");
    }

    /// `install_binary` must succeed when source and destination are on
    /// (potentially) different filesystems — modelled here as different
    /// directories — and must preserve the executable bit. A direct
    /// `rename(src, dest)` across filesystems would fail with EXDEV; the
    /// copy-then-rename path does not.
    #[test]
    fn install_binary_copies_across_directories_and_sets_exec_bit() {
        let src_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();

        let src = src_dir.path().join("undo");
        std::fs::write(&src, b"#!/bin/sh\necho new\n").unwrap();

        let dest = dest_dir.path().join("undo");
        install_binary(&src, &dest).expect("install must succeed across dirs");

        assert_eq!(
            std::fs::read(&dest).unwrap(),
            b"#!/bin/sh\necho new\n",
            "destination must contain the new binary's bytes"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "installed binary must be executable");
        }

        // No staging leftovers next to the destination.
        let staged = staged_path(&dest);
        assert!(!staged.exists(), "staging temp must be cleaned up");
    }

    /// Installing over an existing destination replaces its contents (the
    /// upgrade case), and leaves no staging temp behind.
    #[test]
    fn install_binary_overwrites_existing_destination() {
        let src_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();

        let src = src_dir.path().join("undo");
        std::fs::write(&src, b"NEW").unwrap();

        let dest = dest_dir.path().join("undo");
        std::fs::write(&dest, b"OLD").unwrap();

        install_binary(&src, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"NEW");
    }
}
