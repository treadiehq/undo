use anyhow::{Context, Result};
use std::process::Command;

const REPO: &str = "treadiehq/undo";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn cmd_update() -> Result<()> {
    println!(
        "{}undo{} — self-update",
        crate::BOLD,
        crate::RESET
    );
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

    let tmpdir = std::env::temp_dir().join(format!("undo-update-{}", std::process::id()));
    std::fs::create_dir_all(&tmpdir)?;

    let tarball = tmpdir.join("undo.tar.gz");

    let dl_status = Command::new("curl")
        .args(["-fsSL", &url, "-o"])
        .arg(&tarball)
        .status()
        .context("failed to run curl — is it installed?")?;

    if !dl_status.success() {
        std::fs::remove_dir_all(&tmpdir).ok();
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
        .arg(&tmpdir)
        .status()
        .context("failed to extract archive")?;

    if !tar_status.success() {
        std::fs::remove_dir_all(&tmpdir).ok();
        anyhow::bail!("failed to extract downloaded archive");
    }

    let new_binary = tmpdir.join("undo");
    if !new_binary.exists() {
        std::fs::remove_dir_all(&tmpdir).ok();
        anyhow::bail!("extracted archive does not contain 'undo' binary");
    }

    let current_exe = std::env::current_exe().context("cannot determine current executable path")?;

    // Atomic-ish replace: rename old binary, move new one in, delete old.
    let backup = current_exe.with_extension("old");
    std::fs::rename(&current_exe, &backup)
        .context("failed to replace binary — try running with sudo")?;

    if let Err(e) = std::fs::rename(&new_binary, &current_exe) {
        // Roll back if the move fails.
        std::fs::rename(&backup, &current_exe).ok();
        std::fs::remove_dir_all(&tmpdir).ok();
        return Err(e).context("failed to install new binary");
    }

    std::fs::remove_file(&backup).ok();
    std::fs::remove_dir_all(&tmpdir).ok();

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
