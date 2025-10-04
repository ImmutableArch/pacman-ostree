// pacman_manager.rs
// Pacman Helper functions
use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use std::collections::HashMap;
use std::path::Path;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct PacmanPackageMeta {
    pub pkgname: String,
    pub pkgver: String,
    pub arch: String,
    pub size: u64,
    pub buildtime: u64,
    pub src_pkg: String,
    pub provided_files: Vec<Utf8PathBuf>,
    pub changelogs: Vec<u64>, // unix timestamps
}

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

/// Read all Pacman packages from a commit path (Pacman local database)
/// Read all Pacman packages from a commit path (Pacman local database)
pub(crate) fn read_packages_from_commit(
    repo_path: &Utf8PathBuf,
    _ostree_ref: &str
) -> Result<HashMap<String, PacmanPackageMeta>> {
    let mut packages = HashMap::new();

    // Pacman database path inside the commit
    let db_path = repo_path.join("var/lib/pacman/local");
    if !db_path.exists() {
        anyhow::bail!("Pacman local database path not found: {}", db_path);
    }

    for entry in std::fs::read_dir(&db_path).context("Reading Pacman database directory")? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let desc_file = entry.path().join("desc");
            if desc_file.exists() {
                // Parse desc (core metadata)
                let mut pkg_meta = parse_desc_file(&desc_file)?;

                // Try to read files from "files" file (optional)
                let files_file = entry.path().join("files");
                if files_file.exists() {
                    let files = parse_files_file(&files_file)?;
                    pkg_meta.provided_files = files;
                }

                packages.insert(pkg_meta.pkgname.clone(), pkg_meta);
            }
        }
    }

    Ok(packages)
}

/// Parse a single Pacman desc file into PacmanPackageMeta
fn parse_desc_file(path: &std::path::Path) -> Result<PacmanPackageMeta> {
    let file = File::open(path).context("opening desc file")?;
    let reader = BufReader::new(file);

    let mut current_key = String::new();
    let mut fields: HashMap<String, Vec<String>> = HashMap::new();

    for line in reader.lines() {
        let line = line?;
        if line.starts_with('%') && line.ends_with('%') {
            current_key = line.trim_matches('%').to_string();
            fields.entry(current_key.clone()).or_default();
        } else if !current_key.is_empty() {
            fields.get_mut(&current_key).unwrap().push(line);
        }
    }

    // Extract essential fields
    let pkgname = fields.get("NAME").and_then(|v| v.first()).cloned().unwrap_or_default();
    let pkgver = fields.get("VERSION").and_then(|v| v.first()).cloned().unwrap_or_default();
    let arch = fields.get("ARCH").and_then(|v| v.first()).cloned().unwrap_or_default();
    let size: u64 = fields.get("SIZE").and_then(|v| v.first()).and_then(|s| s.parse().ok()).unwrap_or(0);
    let buildtime: u64 = fields.get("BUILDDATE").and_then(|v| v.first()).and_then(|s| s.parse().ok()).unwrap_or(0);

    Ok(PacmanPackageMeta {
        pkgname: pkgname.clone(),
       pkgver,
       arch,
       size,
       buildtime,
       src_pkg: pkgname,
       provided_files: Vec::new(), // filled later
       changelogs: Vec::new(),     // optional
    })
}

/// Parse the Pacman "files" file into list of paths
fn parse_files_file(path: &std::path::Path) -> Result<Vec<Utf8PathBuf>> {
    let file = File::open(path).context("opening files file")?;
    let reader = BufReader::new(file);

    let mut files = Vec::new();
    let mut in_files_section = false;

    for line in reader.lines() {
        let line = line?;
        if line.trim() == "%FILES%" {
            in_files_section = true;
            continue;
        }

        if in_files_section {
            // Stop when empty line (end of section)
            if line.trim().is_empty() {
                break;
            }
            files.push(Utf8PathBuf::from(format!("/{}", line.trim()))); // add leading slash
        }
    }

    Ok(files)
}

