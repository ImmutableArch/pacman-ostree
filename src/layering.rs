use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::process::Command;

use alpm_utils::alpm_with_conf;
use ostree_ext::ostree;
use ostree_ext::ostree::{RepoCheckoutAtOptions, RepoCommitModifierFlags};
use ostree_ext::prelude::Cast;
use ostree::{gio, Repo, Sysroot};
use pacmanconf::Config;
use serde::{Deserialize, Serialize};
use tar::Archive;
use zstd::stream::read::Decoder;

use crate::solver::SolvResolver;

use crate::solver::{SolvResolver, SolverError};

// ─────────────────────────────────────────────────────────────
// Pacman Client-Side Package Layering
// TODO: - Override package layering (like rpm-ostree overrides)
//       - Custom Kernel support
//       - Per-package rollback
// ─────────────────────────────────────────────────────────────

const OSTREE_REPO_PATH: &str = "/ostree/repo";
const STATE_PATH: &str = "/var/lib/pacman-ostree/state.json";

// ─────────────────────────────────────────────────────────────
// Data structures
// ─────────────────────────────────────────────────────────────

pub struct LayerRequest {
    pub add: Vec<String>,
    pub remove: Vec<String>,
    pub replace: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct LayeredState {
    pub base_commit: String,
    pub layers: Vec<LayerEntry>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum LayerOp {
    Add { pkg: String },
    Remove { pkg: String },
    Replace { pkg: String, spec: ReplaceSpec },
}

#[derive(Serialize, Deserialize, Clone)]
pub struct LayerEntry {
    pub id: String,
    pub op: LayerOp,
    pub commit: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ReplaceSpec {
    pub source: ReplaceSource,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReplaceSource {
    LocalFile(PathBuf),
    RepoPackage { name: String, version: Option<String> },
}

// ─────────────────────────────────────────────────────────────
// Error types
// ─────────────────────────────────────────────────────────────

pub enum DownloadError {
    Alpm(alpm::Error),
    Conf(pacmanconf::Error),
    NotFound(String),
}

impl From<alpm::Error> for DownloadError {
    fn from(e: alpm::Error) -> Self { DownloadError::Alpm(e) }
}

impl From<pacmanconf::Error> for DownloadError {
    fn from(e: pacmanconf::Error) -> Self { DownloadError::Conf(e) }
}

impl<P> From<alpm::AddError<P>> for DownloadError {
    fn from(e: alpm::AddError<P>) -> Self { DownloadError::Alpm(e.into()) }
}

impl<'a> From<alpm::PrepareError<'a>> for DownloadError {
    fn from(e: alpm::PrepareError<'a>) -> Self { DownloadError::Alpm(e.into()) }
}

impl From<alpm::CommitError> for DownloadError {
    fn from(e: alpm::CommitError) -> Self { DownloadError::Alpm(e.into()) }
}

impl fmt::Display for DownloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DownloadError::Alpm(e)     => write!(f, "ALPM error: {}", e),
            DownloadError::Conf(e)     => write!(f, "Config error: {}", e),
            DownloadError::NotFound(p) => write!(f, "Package not found: {}", p),
        }
    }
}

impl fmt::Debug for DownloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { fmt::Display::fmt(self, f) }
}

impl std::error::Error for DownloadError {}

// ─────────────────────────────────────────────────────────────
// State persistence
// ─────────────────────────────────────────────────────────────

/// Wczytuje LayeredState z dysku.
/// Jeśli plik nie istnieje, zwraca pusty state z bieżącym booted commitem jako base.
pub fn load_state() -> Result<LayeredState, Box<dyn Error>> {
    let path = Path::new(STATE_PATH);

    if !path.exists() {
        let sysroot = Sysroot::new_default();
        sysroot.load(gio::Cancellable::NONE)?;
        let booted = sysroot
            .booted_deployment()
            .ok_or("No booted deployment found")?;

        return Ok(LayeredState {
            base_commit: booted.csum().to_string(),
            layers: vec![],
        });
    }

    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let state: LayeredState = serde_json::from_reader(reader)?;
    Ok(state)
}

/// Zapisuje LayeredState na dysk atomowo (write-then-rename).
pub fn save_state(state: &LayeredState) -> Result<(), Box<dyn Error>> {
    let path = Path::new(STATE_PATH);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let tmp_path = path.with_extension("json.tmp");
    {
        let file = File::create(&tmp_path)?;
        let writer = BufWriter::new(file);
        serde_json::to_writer_pretty(writer, state)?;
    }
    std::fs::rename(&tmp_path, path)?;

    println!("[state] Saved to {}", STATE_PATH);
    Ok(())
}

// ─────────────────────────────────────────────────────────────
// OSTree helpers
// ─────────────────────────────────────────────────────────────

pub fn checkout_commit(commit: &str, dest: &Path) -> Result<(), Box<dyn Error>> {
    println!("[ostree] Checking out commit: {}", commit);

    let repo = Repo::open_at(libc::AT_FDCWD, OSTREE_REPO_PATH, gio::Cancellable::NONE)?;

    if dest.exists() {
        std::fs::remove_dir_all(dest)?;
    }
    std::fs::create_dir_all(dest)?;

    let opts = RepoCheckoutAtOptions {
        mode: ostree::RepoCheckoutMode::User,
        overwrite_mode: ostree::RepoCheckoutOverwriteMode::UnionFiles,
        ..Default::default()
    };

    repo.checkout_at(
        Some(&opts),
        libc::AT_FDCWD,
        dest.to_str().ok_or("Invalid dest path")?,
        commit,
        gio::Cancellable::NONE,
    )?;

    Ok(())
}

pub fn commit_layer(
    parent_commit: &str,
    staged_dir: &Path,
    layer_name: &str,
) -> Result<String, Box<dyn Error>> {
    println!("[ostree] Committing layer: {}", layer_name);

    let repo = Repo::open_at(libc::AT_FDCWD, OSTREE_REPO_PATH, gio::Cancellable::NONE)?;

    let modifier = ostree::RepoCommitModifier::new(RepoCommitModifierFlags::NONE, None);
    let mtree = ostree::MutableTree::new();

    let staged_gio = gio::File::for_path(staged_dir);
    repo.write_directory_to_mtree(&staged_gio, &mtree, Some(&modifier), gio::Cancellable::NONE)?;

    let tree_file = repo.write_mtree(&mtree, gio::Cancellable::NONE)?;
    let repo_tree = tree_file
        .downcast_ref::<ostree::RepoFile>()
        .ok_or("Failed to downcast GFile to RepoFile")?;

    let parent_rev = repo.resolve_rev(parent_commit, false)?;

    let checksum = repo.write_commit(
        parent_rev.as_deref(),
        Some(layer_name),
        None,
        None,
        repo_tree,
        gio::Cancellable::NONE,
    )?;

    println!("[ostree] New commit: {}", checksum);
    Ok(checksum.to_string())
}

pub fn deploy_commit(new_checksum: &str) -> Result<(), Box<dyn Error>> {
    println!("[ostree] Deploying commit: {}", new_checksum);

    let sysroot = Sysroot::new_default();
    sysroot.load(gio::Cancellable::NONE)?;

    let booted = sysroot
        .booted_deployment()
        .ok_or("No booted deployment found")?;

    let osname = booted.osname();

    let new_deployment = sysroot.deploy_tree(
        Some(osname.as_str()),
        new_checksum,
        None,
        Some(&booted),
        <&[&str]>::default(),
        gio::Cancellable::NONE,
    )?;

    let flags = ostree::SysrootSimpleWriteDeploymentFlags::RETAIN_ROLLBACK;

    sysroot.simple_write_deployment(
        Some(osname.as_str()),
        &new_deployment,
        Some(&booted),
        flags,
        gio::Cancellable::NONE,
    )?;

    println!("[ostree] Deployment written. Reboot to apply.");
    Ok(())
}

pub fn prune_old_commits() -> Result<(), Box<dyn Error>> {
    println!("[ostree] Pruning old commits...");

    let repo = Repo::open_at(libc::AT_FDCWD, OSTREE_REPO_PATH, gio::Cancellable::NONE)?;

    let (_objects_total, objects_pruned, bytes_freed) = repo.prune(
        ostree::RepoPruneFlags::REFS_ONLY,
        0,
        gio::Cancellable::NONE,
    )?;

    println!(
        "[ostree] Pruned {} objects, freed {} bytes",
        objects_pruned, bytes_freed
    );

    Ok(())
}

// ─────────────────────────────────────────────────────────────
// Dependency & conflict resolution (używa SolvResolver)
// ─────────────────────────────────────────────────────────────

/// Rozwią zuje zależności pakietów używając naszego SolvResolvera.
/// Zwraca listę wszystkich pakietów do zainstalowania (bez duplikatów).
fn resolve_packages(
    packages: &[String],
) -> Result<Vec<String>, Box<dyn Error>> {
    let resolver = SolvResolver::new()
        .map_err(|e| format!("Failed to initialize solver: {}", e))?;
    
    // Rozwiąż każdy pakiet oraz jego zależności
    let mut all_resolved = HashSet::new();
    
    for pkg in packages {
        let result = resolver.resolve_install(&[pkg.clone()])
            .map_err(|e| format!("Failed to resolve '{}': {}", pkg, e))?;
        
        if !result.problems.is_empty() {
            return Err(format!(
                "Dependency conflict for '{}': {}",
                pkg,
                result.problems.join("; ")
            ).into());
        }
        
        all_resolved.extend(result.to_install);
    }
    
    Ok(all_resolved.into_iter().collect())
}

/// Sprawdza konflikty pakietów do instalacji używając SolvResolvera.
fn check_conflicts(
    new_pkgs: &[String],
    _layered_names: &HashSet<String>,
) -> Result<(), Box<dyn Error>> {
    let resolver = SolvResolver::new()
        .map_err(|e| format!("Failed to initialize solver: {}", e))?;
    
    // Sprawdź czy solver wykryje jakieś konflikty
    for pkg in new_pkgs {
        let result = resolver.resolve_install(&[pkg.clone()])
            .map_err(|e| format!("Failed to check conflicts for '{}': {}", pkg, e))?;
        
        if !result.problems.is_empty() {
            return Err(format!(
                "Package '{}' has conflicts: {}",
                pkg,
                result.problems.join("; ")
            ).into());
        }
    }
    
    Ok(())
}

// ────────────────────────────────────────────────────────────────────────────────────

// ─────────────────────────────────────────────────────────────
// Package management
// ─────────────────────────────────────────────────────────────

pub fn install_packages(pkgs: Vec<String>) -> Result<LayeredState, Box<dyn Error>> {
    let mut state = load_state()?;

    // Zbierz pakiety już warstwowane z poprzednich sesji
    let layered_names: HashSet<String> = state
        .layers
        .iter()
        .filter_map(|l| {
            if let LayerOp::Add { pkg } = &l.op {
                Some(pkg.clone())
            } else {
                None
            }
        })
        .collect();

    // Walidacja przed jakąkolwiek operacją na repo
    check_conflicts(&pkgs, &layered_names)?;

    for pkg in &pkgs {
        println!("[pacman-ostree] Processing package: {}", pkg);

        // Użyj SolvResolvera do rozwiązania zależności
        let all_deps = resolve_packages(&[pkg.clone()][..])?;

        if all_deps.is_empty() {
            println!("[pacman-ostree] {} and all deps already present, skipping.", pkg);
            continue;
        }

        let downloaded = download_packages(&all_deps)?;
        let staging_dir = PathBuf::from(format!("/tmp/pacman-ostree-staging-{}", pkg));

        let parent = state
            .layers
            .last()
            .map(|l| l.commit.as_str())
            .unwrap_or(&state.base_commit)
            .to_string();

        checkout_commit(&parent, &staging_dir)?;

        for pkg_path in &downloaded {
            unpack_package_to_staging(pkg_path, &staging_dir)?;
        }

        for dep_name in &all_deps {
            run_post_install_script(dep_name, &staging_dir)?;
        }

        // Pobierz parent ponownie po potencjalnym push do state.layers
        let parent = state
            .layers
            .last()
            .map(|l| l.commit.as_str())
            .unwrap_or(&state.base_commit)
            .to_string();

        let checksum = commit_layer(&parent, &staging_dir, pkg)?;

        state.layers.push(LayerEntry {
            id: pkg.clone(),
            op: LayerOp::Add { pkg: pkg.clone() },
            commit: checksum,
        });

        if staging_dir.exists() {
            std::fs::remove_dir_all(&staging_dir)?;
        }
    }

    if let Some(last_layer) = state.layers.last() {
        deploy_commit(&last_layer.commit)?;
    } else {
        println!("[pacman-ostree] Nothing to deploy.");
    }

    save_state(&state)?;
    prune_old_commits()?;

    Ok(state)
}

pub fn remove_packages(pkgs: &[String]) -> Result<LayeredState, Box<dyn Error>> {
    let mut state = load_state()?;

    let to_remove: HashSet<String> = pkgs.iter().cloned().collect();

    // Sprawdź czy wszystkie pakiety do usunięcia faktycznie są warstwowane
    for pkg in pkgs {
        let exists = state.layers.iter().any(|l| {
            matches!(&l.op, LayerOp::Add { pkg: p } if p == pkg)
        });
        if !exists {
            return Err(format!("Package '{}' is not layered", pkg).into());
        }
    }

    // Zbierz pakiety które zostają w oryginalnej kolejności
    let remaining_pkgs: Vec<String> = state
        .layers
        .iter()
        .filter_map(|l| {
            if let LayerOp::Add { pkg } = &l.op {
                if !to_remove.contains(pkg) {
                    return Some(pkg.clone());
                }
            }
            None
        })
        .collect();

    // Wyczyść stare warstwy — odbudujemy je od base
    state.layers.clear();

    if remaining_pkgs.is_empty() {
        println!("[pacman-ostree] No layers remaining, deploying base commit.");
        deploy_commit(&state.base_commit)?;
        save_state(&state)?;
        prune_old_commits()?;
        return Ok(state);
    }

    // Replay: odbuduj każdą pozostałą warstwę od base
    for pkg in &remaining_pkgs {
        println!("[pacman-ostree] Replaying layer: {}", pkg);

        let all_deps = resolve_packages(&[pkg.clone()][..])?;

        if all_deps.is_empty() {
            continue;
        }

        let downloaded = download_packages(&all_deps)?;
        let staging_dir = PathBuf::from(format!("/tmp/pacman-ostree-replay-{}", pkg));

        let parent = state
            .layers
            .last()
            .map(|l| l.commit.as_str())
            .unwrap_or(&state.base_commit)
            .to_string();

        checkout_commit(&parent, &staging_dir)?;

        for pkg_path in &downloaded {
            unpack_package_to_staging(pkg_path, &staging_dir)?;
        }

        for dep_name in &all_deps {
            run_post_install_script(dep_name, &staging_dir)?;
        }

        // Pobierz parent ponownie po potencjalnym push
        let parent = state
            .layers
            .last()
            .map(|l| l.commit.as_str())
            .unwrap_or(&state.base_commit)
            .to_string();

        let checksum = commit_layer(&parent, &staging_dir, pkg)?;

        state.layers.push(LayerEntry {
            id: pkg.clone(),
            op: LayerOp::Add { pkg: pkg.clone() },
            commit: checksum,
        });

        if staging_dir.exists() {
            std::fs::remove_dir_all(&staging_dir)?;
        }
    }

    let deploy_target = state
        .layers
        .last()
        .map(|l| l.commit.as_str())
        .unwrap_or(&state.base_commit)
        .to_string();

    deploy_commit(&deploy_target)?;
    save_state(&state)?;
    prune_old_commits()?;

    Ok(state)
}

// ─────────────────────────────────────────────────────────────
// Internal helpers
// ─────────────────────────────────────────────────────────────

fn unpack_package_to_staging(pkg_path: &str, staging_dir: &Path) -> Result<(), Box<dyn Error>> {
    std::fs::create_dir_all(staging_dir)?;

    println!("[unpack] Unpacking {} -> {}", pkg_path, staging_dir.display());

    if pkg_path.ends_with(".tar.zst") {
        let file = File::open(pkg_path)?;
        let decoder = Decoder::new(file)?;
        let mut archive = Archive::new(decoder);
        archive.unpack(staging_dir)?;
    } else if pkg_path.ends_with(".tar.xz") {
        let status = Command::new("tar")
            .args(["-xJf", pkg_path, "-C", staging_dir.to_str().unwrap()])
            .status()?;
        if !status.success() {
            return Err(format!("tar failed for {}", pkg_path).into());
        }
    } else if pkg_path.ends_with(".tar.gz") {
        let status = Command::new("tar")
            .args(["-xzf", pkg_path, "-C", staging_dir.to_str().unwrap()])
            .status()?;
        if !status.success() {
            return Err(format!("tar failed for {}", pkg_path).into());
        }
    } else {
        return Err(format!("Unknown package format: {}", pkg_path).into());
    }

    for meta in &[".PKGINFO", ".MTREE", ".BUILDINFO"] {
        let path = staging_dir.join(meta);
        if path.exists() {
            std::fs::remove_file(path)?;
        }
    }

    Ok(())
}

fn run_post_install_script(pkg_name: &str, staging_dir: &Path) -> Result<(), Box<dyn Error>> {
    let install_script = staging_dir.join(".INSTALL");

    if !install_script.exists() {
        return Ok(());
    }

    println!("[post-install] Running post_install for {}", pkg_name);

    let staging = staging_dir.to_str().ok_or("Invalid staging path")?;

    let script = r#"
        . /.INSTALL
        if declare -f post_install > /dev/null 2>&1; then
            post_install
        fi
    "#;

    let status = Command::new("bwrap")
        .args([
            "--bind",    staging, "/",
            "--proc",    "/proc",
            "--dev",     "/dev",
            "--ro-bind", "/sys", "/sys",
            "--unshare-net",
            "--unshare-all",
            "--die-with-parent",
            "--",
            "/bin/sh", "-c", script,
        ])
        .status()?;

    if !status.success() {
        return Err(format!("post_install script failed for package: {}", pkg_name).into());
    }

    std::fs::remove_file(install_script)?;

    Ok(())
}

pub fn download_packages(pkgs: &[String]) -> Result<Vec<String>, DownloadError> {
    let conf = Config::new()?;
    let mut alpm = alpm_with_conf(&conf)?;

    let cache_dir = conf
        .cache_dir
        .first()
        .map(|s| s.as_str())
        .unwrap_or("/var/cache/pacman/pkg")
        .to_string();

    alpm.trans_init(alpm::TransFlag::DOWNLOAD_ONLY)?;

    for name in pkgs {
        let mut found = false;
        for db in alpm.syncdbs().iter() {
            if let Ok(pkg) = db.pkg(name.clone()) {
                alpm.trans_add_pkg(pkg)?;
                found = true;
                break;
            }
        }
        if !found {
            return Err(DownloadError::NotFound(name.clone()));
        }
    }

    alpm.trans_prepare()?;
    alpm.trans_commit()?;

    let downloaded: Vec<String> = alpm
        .trans_add()
        .iter()
        .filter_map(|pkg| pkg.filename())
        .map(|filename| format!("{}/{}", cache_dir, filename))
        .collect();

    alpm.trans_release()?;

    Ok(downloaded)
}