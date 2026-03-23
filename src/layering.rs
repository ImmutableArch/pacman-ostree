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

// Derive Serialize/Deserialize żeby można było zapisać do JSON
#[derive(Serialize, Deserialize, Clone)]
pub struct LayeredState {
    pub base_commit: String,
    pub layers: Vec<LayerEntry>,
}

// LayerOp musi być serializowalne — używamy tagged enum
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
        // Pierwsza instalacja — zbuduj stan na podstawie bieżącego deploymentu
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

    // Utwórz katalog jeśli nie istnieje
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Zapis atomowy: piszemy do pliku tymczasowego, potem rename
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

/// Checkoutuje commit OSTree do katalogu `dest`.
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

/// Commituje zawartość `staged_dir` jako nową warstwę na bazie `parent_commit`.
/// Zwraca checksum nowego commitu.
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

/// Deployuje podany commit jako nowy deployment (zachowując poprzedni jako rollback).
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

/// Usuwa stare commity z repo — zostawia tylko commity osiągalne z aktywnych
/// deploymentów (bieżący + rollback).
pub fn prune_old_commits() -> Result<(), Box<dyn Error>> {
    println!("[ostree] Pruning old commits...");

    let repo = Repo::open_at(libc::AT_FDCWD, OSTREE_REPO_PATH, gio::Cancellable::NONE)?;

    // REFS_ONLY = nie usuwaj commitów osiągalnych z aktywnych refs/deploymentów
    // depth 0 = zostaw tylko commity bezpośrednio wskazywane przez deploymenty
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
// Package management
// ─────────────────────────────────────────────────────────────

/// Instaluje listę pakietów jako warstwy.
/// Wczytuje istniejący stan z dysku, dodaje nowe warstwy, zapisuje i deployuje.
pub fn install_packages(pkgs: Vec<String>) -> Result<LayeredState, Box<dyn Error>> {
    let mut state = load_state()?;

    // Zbierz pakiety już warstwowane z poprzednich sesji
    let mut already_layered: HashSet<String> = state
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

    let base_installed = get_base_installed_packages()?;

    for pkg in &pkgs {
        println!("[pacman-ostree] Processing package: {}", pkg);

        let mut all_deps = resolve_packages(vec![pkg.clone()])?;
        all_deps.retain(|p| !base_installed.contains(p) && !already_layered.contains(p));

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
            .unwrap_or(&state.base_commit);

        checkout_commit(parent, &staging_dir)?;

        for pkg_path in &downloaded {
            unpack_package_to_staging(pkg_path, &staging_dir)?;
        }

        for dep_name in &all_deps {
            run_post_install_script(dep_name, &staging_dir)?;
        }

        let parent = state
            .layers
            .last()
            .map(|l| l.commit.as_str())
            .unwrap_or(&state.base_commit);

        let checksum = commit_layer(parent, &staging_dir, pkg)?;

        state.layers.push(LayerEntry {
            id: pkg.clone(),
            op: LayerOp::Add { pkg: pkg.clone() },
            commit: checksum,
        });

        already_layered.extend(all_deps);

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

/// Usuwa pakiet z warstw przez replay wszystkich pozostałych layerów od base commitu.
///
/// Strategia:
/// 1. Załaduj stan
/// 2. Usuń wpis dla `pkg_name` z listy warstw
/// 3. Odtwórz wszystkie pozostałe warstwy od zera (checkout base → unpack każdej)
/// 4. Zdeploy nowy ostatni commit
/// 5. Zapisz stan
pub fn remove_packages(pkgs: &[String]) -> Result<LayeredState, Box<dyn Error>> {
    let mut state = load_state()?;

    let to_remove: HashSet<String> = pkgs.iter().cloned().collect();

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

    state.layers.clear();

    if remaining_pkgs.is_empty() {
        deploy_commit(&state.base_commit)?;
        save_state(&state)?;
        prune_old_commits()?;
        return Ok(state);
    }

    let base_installed = get_base_installed_packages()?;
    let mut already_layered: HashSet<String> = HashSet::new();

    for pkg in &remaining_pkgs {
        let mut all_deps = resolve_packages(vec![pkg.clone()])?;
        all_deps.retain(|p| !base_installed.contains(p) && !already_layered.contains(p));

        if all_deps.is_empty() {
            continue;
        }

        let downloaded = download_packages(&all_deps)?;
        let staging_dir = PathBuf::from(format!("/tmp/pacman-ostree-replay-{}", pkg));

        let parent = state
            .layers
            .last()
            .map(|l| l.commit.as_str())
            .unwrap_or(&state.base_commit);

        checkout_commit(parent, &staging_dir)?;

        for pkg_path in &downloaded {
            unpack_package_to_staging(pkg_path, &staging_dir)?;
        }

        for dep_name in &all_deps {
            run_post_install_script(dep_name, &staging_dir)?;
        }

        let checksum = commit_layer(parent, &staging_dir, pkg)?;

        state.layers.push(LayerEntry {
            id: pkg.clone(),
            op: LayerOp::Add { pkg: pkg.clone() },
            commit: checksum,
        });

        already_layered.extend(all_deps);

        if staging_dir.exists() {
            std::fs::remove_dir_all(&staging_dir)?;
        }
    }

    let deploy_target = state
        .layers
        .last()
        .map(|l| l.commit.as_str())
        .unwrap_or(&state.base_commit);

    deploy_commit(deploy_target)?;
    save_state(&state)?;
    prune_old_commits()?;

    Ok(state)
}

// ─────────────────────────────────────────────────────────────
// Internal helpers
// ─────────────────────────────────────────────────────────────

fn get_base_installed_packages() -> Result<HashSet<String>, Box<dyn Error>> {
    let conf = Config::new()?;
    let alpm = alpm_with_conf(&conf)?;
    let installed: HashSet<String> = alpm
        .localdb()
        .pkgs()
        .iter()
        .map(|p| p.name().to_string())
        .collect();
    Ok(installed)
}

fn resolve_packages(initial: Vec<String>) -> Result<Vec<String>, Box<dyn Error>> {
    let conf = Config::new()?;
    let alpm = alpm_with_conf(&conf)?;
    let syncdbs = alpm.syncdbs();

    let mut resolved: HashSet<String> = HashSet::new();
    let mut queue = initial;

    while let Some(current) = queue.pop() {
        if resolved.contains(&current) {
            continue;
        }

        let pkg = syncdbs
            .iter()
            .filter_map(|db| db.pkg(current.clone()).ok())
            .next()
            .ok_or_else(|| format!("Package not found in syncdbs: {}", current))?;

        resolved.insert(current);

        for dep in pkg.depends().iter() {
            let dep_name = dep.name().to_string();
            if !resolved.contains(&dep_name) {
                queue.push(dep_name);
            }
        }
    }

    Ok(resolved.into_iter().collect())
}

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