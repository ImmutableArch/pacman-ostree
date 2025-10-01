// pacman_manager.rs
// Pacman Helper functions
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

/// Install packages using pacman into given root.
/// Returns error with full stdout/stderr when the command fails.
pub(crate) fn install(root: &Path, cache: &str, packages: &[String]) -> Result<()> {
    // -- English comments inside code as requested --
    // Run pacman -Sy -r <root> --cachedir=<cache> --noconfirm <packages...>
    let output = Command::new("pacman")
        .arg("-Sy")
        .arg("-r")
        .arg(root)
        .arg(format!("--cachedir={}", cache))
        .arg("--noconfirm")
        .args(packages)
        .output()
        .context("Failed to spawn pacman for install")?;

    if !output.status.success() {
        anyhow::bail!(
            "pacman install failed (status: {:?})\nstdout:\n{}\nstderr:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    println!("pacman install finished OK\nstdout:\n{}", String::from_utf8_lossy(&output.stdout));
    Ok(())
}

/// Remove packages using pacman from given root.
pub(crate) fn remove(root: &Path, cache: &str, packages: &[String]) -> Result<()> {
    let output = Command::new("pacman")
        .arg("-Rns")
        .arg("-r")
        .arg(root)
        .arg(format!("--cachedir={}", cache))
        .arg("--noconfirm")
        .args(packages)
        .output()
        .context("Failed to spawn pacman for remove")?;

    if !output.status.success() {
        anyhow::bail!(
            "pacman remove failed (status: {:?})\nstdout:\n{}\nstderr:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    println!("pacman remove finished OK\nstdout:\n{}", String::from_utf8_lossy(&output.stdout));
    Ok(())
}

/// Run pacstrap to populate the root filesystem.
/// This captures stdout/stderr and returns a detailed error on failure.
pub(crate) fn pacstrap_install(root: &Path, packages: &[String]) -> Result<()> {
    // ensure running as root (pacstrap generally requires root)
    if unsafe { libc::geteuid() } != 0 {
        anyhow::bail!("pacstrap_install requires root privileges (EUID != 0)");
    }

    // `pacstrap -c <root> --noconfirm <packages...>`
    let output = Command::new("pacstrap")
        .arg("-c")
        .arg(root)
        .arg("--noconfirm")
        .args(packages)
        .output()
        .context("Failed to spawn pacstrap")?;

    if !output.status.success() {
        anyhow::bail!(
            "pacstrap failed (status: {:?})\nstdout:\n{}\nstderr:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    println!("pacstrap finished OK\nstdout:\n{}", String::from_utf8_lossy(&output.stdout));
    Ok(())
}
