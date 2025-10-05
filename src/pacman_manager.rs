// pacman_manager.rs
// Pacman Helper functions
use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use std::collections::HashMap;
use std::path::Path;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::process::Command;
use ostree_ext::ostree;
use ostree::gio;
use std::io::Cursor;
use ostree_ext::prelude::*;
use std::str;

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
/// This implementation opens the OSTree repo, reads the commit, and looks
/// for var/lib/pacman/local inside the commit tree.
pub(crate) fn read_packages_from_commit(
    repo_path: &Utf8PathBuf,
    ostree_ref: &str
) -> Result<HashMap<String, PacmanPackageMeta>> {
    let mut packages = HashMap::new();

    // Open ostree repo (same approach used elsewhere in your code)
    let repo = ostree::Repo::open_at(libc::AT_FDCWD, repo_path.as_str(), gio::Cancellable::NONE)
        .context("Opening OSTree repo")?;

    // Read the commit (this gives us a gio::File representing the commit root tree)
    let (root, _rev) = repo
        .read_commit(ostree_ref, gio::Cancellable::NONE)
        .with_context(|| format!("Reading commit '{}' from repo '{}'", ostree_ref, repo_path.as_str()))?;

    // Build the path inside the commit: /var/lib/pacman/local
    let var = root.child("var");
    let lib = var.child("lib");
    let pacman = lib.child("pacman");
    let local = pacman.child("local");

    if !local.query_exists(gio::Cancellable::NONE) {
        anyhow::bail!(
            "Pacman local database path not found in commit: {}/var/lib/pacman/local",
            repo_path.as_str()
        );
    }

    // enumerate children (each subdir is a package directory)
    let enumerator = local
        .enumerate_children(
            "standard::name,standard::type",
            gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
            gio::Cancellable::NONE,
        )
        .context("Reading pacman local directory entries")?;

    for entry in enumerator {
        let entry = entry?;
        if entry.file_type() == gio::FileType::Directory {
            let pkgdir_name = entry.name();
            let pkgdir = local.child(&pkgdir_name);

            // desc file is required for pacman package metadata
            let desc = pkgdir.child("desc");
            if !desc.query_exists(gio::Cancellable::NONE) {
                // skip packages without desc
                continue;
            }

            // load desc contents
            let (desc_bytes, _) = desc
                .load_contents(gio::Cancellable::NONE)
                .with_context(|| format!("Loading desc for package dir '{}'", pkgdir_name.display()))?;
            let mut pkg_meta = parse_desc_from_bytes(&desc_bytes)
                .with_context(|| format!("Parsing desc for package '{}'", pkgdir_name.display()))?;

            // optionally parse files file
            let files_file = pkgdir.child("files");
            if files_file.query_exists(gio::Cancellable::NONE) {
                let (files_bytes, _) = files_file
                    .load_contents(gio::Cancellable::NONE)
                    .with_context(|| format!("Loading files for package '{}'", pkgdir_name.display()))?;
                let files = parse_files_from_bytes(&files_bytes)
                    .with_context(|| format!("Parsing files for package '{}'", pkgdir_name.display()))?;
                pkg_meta.provided_files = files;
            }

            packages.insert(pkg_meta.pkgname.clone(), pkg_meta);
        }
    }

    Ok(packages)
}

/// Parse a single Pacman desc content (from bytes) into PacmanPackageMeta
fn parse_desc_from_bytes(bytes: &[u8]) -> Result<PacmanPackageMeta> {
    let cursor = Cursor::new(bytes);
    let reader = BufReader::new(cursor);

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

/// Parse the Pacman "files" content (from bytes) into list of paths
fn parse_files_from_bytes(bytes: &[u8]) -> Result<Vec<Utf8PathBuf>> {
    let cursor = Cursor::new(bytes);
    let reader = BufReader::new(cursor);

    let mut files = Vec::new();
    let mut in_files_section = false;

    for line in reader.lines() {
        let line = line?;
        let t = line.trim();
        if t == "%FILES%" {
            in_files_section = true;
            continue;
        }

        if in_files_section {
            // Stop when empty line (end of section)
            if t.is_empty() {
                break;
            }
            // Ensure leading slash, like pacman local files do store relative paths
            let path = if t.starts_with('/') { t.to_string() } else { format!("/{}", t) };
            files.push(Utf8PathBuf::from(path));
        }
    }

    Ok(files)
}

