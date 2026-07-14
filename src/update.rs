use anyhow::{Context, Result};
use semver::Version;
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::process::Command;

const REPO: &str = "treadiehq/undo";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn cmd_update() -> Result<()> {
    println!("{}undo{} — self-update", crate::BOLD, crate::RESET);
    println!();
    println!("  Current version: v{}", CURRENT_VERSION);
    println!("Checking for updates...");

    let latest_tag = fetch_latest_tag()?;
    let latest_ver = latest_tag.strip_prefix('v').unwrap_or(&latest_tag);

    println!("  Latest release:  {}", latest_tag);
    println!();

    match compare_release_versions(latest_ver, CURRENT_VERSION)? {
        Ordering::Greater => {}
        Ordering::Equal => {
            println!("Already up to date.");
            return Ok(());
        }
        Ordering::Less => {
            println!("Current version is newer than the latest GitHub release; not downgrading.");
            return Ok(());
        }
    }

    let target = detect_target()?;
    let url = format!(
        "https://github.com/{REPO}/releases/download/{latest_tag}/undo-{latest_tag}-{target}.tar.gz"
    );

    println!("Downloading undo {} for {}...", latest_tag, target);

    let tmpdir = tempfile::TempDir::new()
        .context("Downloading update failed: could not create a temporary directory")?;

    let tarball = tmpdir.path().join("undo.tar.gz");

    let dl_status = Command::new("curl")
        .args(["-fsSL", &url, "-o"])
        .arg(&tarball)
        .status()
        .context("Downloading update failed: could not start curl; install curl and try again")?;

    if !dl_status.success() {
        anyhow::bail!(
            "Downloading update failed: GitHub did not provide release {} for {}.",
            latest_tag,
            target
        );
    }

    // Verify integrity before we extract or install anything (#33). For a tool
    // that overwrites its own executable, refusing to install an artifact we
    // can't verify is the expected posture.
    println!("Verifying download...");
    verify_checksum(&tarball, &latest_tag, &target, tmpdir.path())?;

    println!("Installing update...");
    let tar_status = Command::new("tar")
        .args(["xzf"])
        .arg(&tarball)
        .arg("-C")
        .arg(tmpdir.path())
        .status()
        .context("Installing update failed: could not start tar to extract the download")?;

    if !tar_status.success() {
        anyhow::bail!("Installing update failed: tar could not extract the downloaded archive");
    }

    let new_binary = tmpdir.path().join("undo");
    if !new_binary.exists() {
        anyhow::bail!("Installing update failed: the downloaded archive has no 'undo' binary");
    }

    let current_exe = std::env::current_exe()
        .context("Installing update failed: could not find the current executable")?;

    // Move the old binary aside, then install the new one. Both steps must work
    // even though `new_binary` lives in a tempdir that is very often on a
    // DIFFERENT filesystem than the install location (e.g. `/tmp` is tmpfs on
    // most Linux distros; `$TMPDIR` is a separate APFS volume on macOS). A bare
    // `rename` across filesystems fails with EXDEV, which silently broke every
    // self-update in those (very common) setups.
    let backup = current_exe.with_extension("old");
    std::fs::rename(&current_exe, &backup).with_context(|| {
        format!(
            "Installing update failed: could not replace {}. Check that you own the file and its directory",
            current_exe.display()
        )
    })?;

    if let Err(e) = install_binary(&new_binary, &current_exe) {
        std::fs::rename(&backup, &current_exe).ok();
        return Err(e).context("Installing update failed");
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
        .context("Checking for updates failed: could not start curl; install curl and try again")?;

    if !output.status.success() {
        anyhow::bail!("Checking for updates failed: GitHub did not return the latest release");
    }

    let body = String::from_utf8_lossy(&output.stdout);

    // Minimal JSON parsing to avoid adding serde_json as a dependency.
    // Looks for "tag_name": "v0.1.2"
    let tag = body
        .split("\"tag_name\"")
        .nth(1)
        .and_then(|s| s.split('"').nth(1))
        .map(|s| s.to_string())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Checking for updates failed: GitHub returned no readable release version"
            )
        })?;

    Ok(tag)
}

fn parse_release_version(version: &str) -> Result<Version> {
    let version = version.strip_prefix('v').unwrap_or(version);
    Version::parse(version).with_context(|| {
        format!(
            "Checking for updates failed: release version '{version}' is not a valid semantic version"
        )
    })
}

fn compare_release_versions(latest: &str, current: &str) -> Result<Ordering> {
    Ok(parse_release_version(latest)?.cmp(&parse_release_version(current)?))
}

/// Verify the downloaded tarball against the release's published `SHA256SUMS`
/// before it is extracted or installed (#33). A missing checksum file, a missing
/// entry for this artifact, or a hash mismatch all abort the update — we never
/// install bytes we couldn't verify.
fn verify_checksum(
    tarball: &std::path::Path,
    tag: &str,
    target: &str,
    tmpdir: &std::path::Path,
) -> Result<()> {
    let sums_url = format!("https://github.com/{REPO}/releases/download/{tag}/SHA256SUMS");
    let sums_path = tmpdir.join("SHA256SUMS");

    let status = Command::new("curl")
        .args(["-fsSL", &sums_url, "-o"])
        .arg(&sums_path)
        .status()
        .context("Verifying download failed: could not start curl; install curl and try again")?;
    if !status.success() {
        anyhow::bail!(
            "Verifying download failed: could not download SHA256SUMS for {tag}; the update was not installed"
        );
    }

    let sums = std::fs::read_to_string(&sums_path)
        .context("Verifying download failed: could not read SHA256SUMS")?;
    let bytes = std::fs::read(tarball)
        .context("Verifying download failed: could not read the downloaded archive")?;
    let actual = crate::to_hex(&Sha256::digest(&bytes));

    let artifact = format!("undo-{tag}-{target}.tar.gz");
    verify_against_sums(&sums, &artifact, &actual)?;

    println!("  Verified SHA-256 checksum.");
    Ok(())
}

/// Pure check: confirm `SHA256SUMS` lists `artifact` with hash `actual_hex`.
///
/// Accepts the GNU `sha256sum` text format (`<hex>  <name>`) and the binary
/// marker form (`<hex> *<name>`), and matches on the file's basename so a
/// path-qualified entry still resolves. Kept I/O-free so it can be unit-tested.
fn verify_against_sums(sums: &str, artifact: &str, actual_hex: &str) -> Result<()> {
    let expected = sums
        .lines()
        .find_map(|line| {
            let mut parts = line.split_whitespace();
            let hash = parts.next()?;
            let name = parts.next()?.trim_start_matches('*');
            let base = name.rsplit('/').next().unwrap_or(name);
            (base == artifact).then(|| hash.to_ascii_lowercase())
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Verifying download failed: SHA256SUMS has no entry for {artifact}; the update was not installed"
            )
        })?;

    if actual_hex.to_ascii_lowercase() != expected {
        anyhow::bail!(
            "Verifying download failed: checksum mismatch for {artifact}\n  expected: {expected}\n  actual:   {actual_hex}\n\
             The download may be corrupt or tampered with; the update was not installed."
        );
    }

    Ok(())
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
    result.context("could not stage and install the new binary")
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
        _ => anyhow::bail!(
            "Downloading update failed: operating system '{}' is not supported",
            os
        ),
    };

    let arch_part = match arch {
        "aarch64" => "aarch64",
        "x86_64" => "x86_64",
        _ => anyhow::bail!(
            "Downloading update failed: architecture '{}' is not supported",
            arch
        ),
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

    /// GitHub's `/releases/latest` is based on creation time, not semantic
    /// ordering. If it reports an older patch release, self-update must not
    /// install it over a newer local binary.
    #[test]
    fn version_comparison_prevents_patch_downgrade() {
        assert_eq!(
            compare_release_versions("v0.1.2", "0.1.14").unwrap(),
            Ordering::Less
        );
    }

    /// Multi-digit version components must compare numerically, not as strings.
    #[test]
    fn version_comparison_handles_multi_digit_components() {
        assert_eq!(
            compare_release_versions("v1.10.0", "1.9.0").unwrap(),
            Ordering::Greater
        );
        assert_eq!(
            compare_release_versions("v10.0.0", "2.0.0").unwrap(),
            Ordering::Greater
        );
    }

    /// Equal versions, with or without a leading `v`, are not updates.
    #[test]
    fn version_comparison_treats_matching_tags_as_equal() {
        assert_eq!(
            compare_release_versions("v0.1.14", "0.1.14").unwrap(),
            Ordering::Equal
        );
    }

    /// A malformed release tag leaves version ordering unknowable, so update
    /// must fail closed before any download/install work begins.
    #[test]
    fn version_comparison_rejects_invalid_release_tags() {
        let err = compare_release_versions("latest", "0.1.14")
            .unwrap_err()
            .to_string();
        assert!(err.contains("valid semantic version"), "got: {err}");
    }

    /// A matching checksum line verifies the artifact (#33). Hash comparison is
    /// case-insensitive and ignores other entries in the file.
    #[test]
    fn verify_against_sums_accepts_matching_hash() {
        let sums = "\
aaaa  undo-v1.0.0-x86_64-unknown-linux-gnu.tar.gz
bbbb  undo-v1.0.0-aarch64-apple-darwin.tar.gz
";
        assert!(
            verify_against_sums(sums, "undo-v1.0.0-aarch64-apple-darwin.tar.gz", "BBBB").is_ok(),
            "matching hash (case-insensitive) must verify"
        );
    }

    /// A wrong hash for an otherwise-present artifact must fail.
    #[test]
    fn verify_against_sums_rejects_mismatch() {
        let sums = "dead  undo-v1.0.0-x86_64-apple-darwin.tar.gz\n";
        let err = verify_against_sums(sums, "undo-v1.0.0-x86_64-apple-darwin.tar.gz", "beef")
            .unwrap_err()
            .to_string();
        assert!(err.contains("checksum mismatch"), "got: {err}");
    }

    /// No entry for the artifact must fail closed (strict posture) rather than
    /// silently installing an unverified binary.
    #[test]
    fn verify_against_sums_rejects_missing_entry() {
        let sums = "dead  some-other-file.tar.gz\n";
        let err = verify_against_sums(sums, "undo-v1.0.0-x86_64-apple-darwin.tar.gz", "dead")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no entry"), "got: {err}");
    }

    /// The binary-marker form (`<hex> *<name>`) that `sha256sum -b` emits must
    /// still parse and match.
    #[test]
    fn verify_against_sums_handles_binary_marker_format() {
        let sums = "c0ffee *undo-v2.0.0-x86_64-unknown-linux-gnu.tar.gz\n";
        assert!(
            verify_against_sums(
                sums,
                "undo-v2.0.0-x86_64-unknown-linux-gnu.tar.gz",
                "c0ffee"
            )
            .is_ok()
        );
    }
}
