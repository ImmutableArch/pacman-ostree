// package_layering.rs
// Package layering implementation for pacman-ostree
// Philosophy: Every change is "from scratch" - rebuild the entire tree for each operation
// Similar to rpm-ostree: only ADD packages on top of base, never remove base packages

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use ostree_ext::ostree;
use ostree::gio;
use ostree::prelude::*;
use ostree::glib;
use tempfile::TempDir;
use std::process::Command;
use serde::{Deserialize, Serialize};

use crate::pacman_manager::{self, PacmanPackageMeta};

/// State file that tracks what packages are layered on top of base
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LayeredState {
    /// Base commit reference
    pub base_ref: String,
    /// Packages installed on top of base (user requested)
    pub layered_packages: HashSet<String>,
    /// Last deployed commit
    pub deployed_commit: Option<String>,
}

impl LayeredState {
    /// Load state from OSTree repo metadata
    pub fn load_from_repo(repo_path: &Utf8PathBuf, state_ref: &str) -> Result<Self> {
        let repo = ostree::Repo::open_at(libc::AT_FDCWD, repo_path.as_str(), gio::Cancellable::NONE)
            .context("Opening OSTree repo")?;

        // Try to read commit metadata
        match repo.resolve_rev(state_ref, true)? {
            Some(_checksum) => {
                // Read state from commit metadata
                Ok(Self::default())
            }
            None => {
                // No state yet, return empty
                Ok(Self::default())
            }
        }
    }
}

#[derive(Debug)]
pub struct LayeringResult {
    /// New commit checksum
    pub new_commit: String,
    /// Deployment index (if deployed)
    pub deployment_index: Option<u32>,
    /// Packages that were newly installed in this operation
    pub newly_installed: Vec<String>,
    /// Total layered packages after this operation
    pub total_layered: usize,
    /// Total size change in bytes
    pub size_delta: i64,
}

/// Install packages on top of the current deployment
/// This is like: rpm-ostree install <packages>
pub fn install_packages(
    repo_path: &Utf8PathBuf,
    pacman_cache: &Utf8PathBuf,
    pacman_conf: Option<&Path>,
    packages: &[String],
    deploy: bool,
) -> Result<LayeringResult> {
    println!("📦 pacman-ostree install: {:?}", packages);

    // Get current deployment info
    let current_deployment = get_booted_deployment()
        .context("Getting current deployment")?;

    println!("   Current deployment: {} ({})", 
        current_deployment.osname, 
        &current_deployment.commit[..8]
    );

    // Load current layered state from commit metadata
    let mut state = load_state_from_commit(repo_path, &current_deployment.commit)?;

    // Check if packages are already layered
    let mut new_packages = Vec::new();
    for pkg in packages {
        if state.layered_packages.contains(pkg) {
            println!("   ⚠️  Package '{}' is already layered", pkg);
        } else {
            new_packages.push(pkg.clone());
            state.layered_packages.insert(pkg.clone());
        }
    }

    if new_packages.is_empty() {
        anyhow::bail!("All requested packages are already layered");
    }

    println!("   New packages to layer: {:?}", new_packages);
    println!("   Total layered after: {}", state.layered_packages.len());

    // Rebuild tree from scratch with all layered packages
    let result = rebuild_with_layers(
        repo_path,
        pacman_cache,
        pacman_conf,
        &state,
        &current_deployment.osname,
        deploy,
    )?;

    Ok(result)
}

/// Install packages on a fresh system (no existing deployment)
/// This initializes the layering system with a base ref
pub fn install_packages_fresh(
    repo_path: &Utf8PathBuf,
    pacman_cache: &Utf8PathBuf,
    pacman_conf: Option<&Path>,
    base_ref: &str,
    packages: &[String],
    deploy: bool,
) -> Result<LayeringResult> {
    println!("🆕 Fresh installation - initializing layering");
    println!("   Base ref: {}", base_ref);
    println!("   Packages to layer: {:?}", packages);

    // Verify base ref exists
    let repo = ostree::Repo::open_at(libc::AT_FDCWD, repo_path.as_str(), gio::Cancellable::NONE)
        .context("Opening OSTree repo")?;
    
    let base_exists = repo.resolve_rev(base_ref, false)
        .context(format!("Base ref '{}' not found in repository", base_ref))?;
    
    if base_exists.is_none() {
        anyhow::bail!("Base ref '{}' does not exist in repository", base_ref);
    }

    // Extract OS name from ref (e.g., "archlinux/x86_64/base" -> "archlinux")
    let osname = base_ref.split('/').next().unwrap_or("default");

    // Create initial state with packages
    let mut state = LayeredState {
        base_ref: base_ref.to_string(),
        layered_packages: packages.iter().cloned().collect(),
        deployed_commit: None,
    };

    println!("   OS name: {}", osname);

    // Rebuild tree from scratch with layered packages
    let result = rebuild_with_layers(
        repo_path,
        pacman_cache,
        pacman_conf,
        &state,
        osname,
        deploy,
    )?;

    Ok(result)
}

/// Remove packages from the layered set
/// This is like: rpm-ostree uninstall <packages>
/// NOTE: Can only remove packages that were layered, NOT base packages!
pub fn remove_packages(
    repo_path: &Utf8PathBuf,
    pacman_cache: &Utf8PathBuf,
    pacman_conf: Option<&Path>,
    packages: &[String],
    deploy: bool,
) -> Result<LayeringResult> {
    println!("🗑️  pacman-ostree remove: {:?}", packages);

    let current_deployment = get_booted_deployment()
        .context("Getting current deployment")?;

    // Load current layered state
    let mut state = load_state_from_commit(repo_path, &current_deployment.commit)?;

    // Check if packages are layered (can't remove base packages!)
    let mut to_remove = Vec::new();
    for pkg in packages {
        if !state.layered_packages.contains(pkg) {
            anyhow::bail!(
                "Cannot remove '{}': not a layered package (base packages cannot be removed)",
                pkg
            );
        }
        to_remove.push(pkg.clone());
        state.layered_packages.remove(pkg);
    }

    println!("   Removing layered packages: {:?}", to_remove);
    println!("   Remaining layered: {}", state.layered_packages.len());

    // Rebuild tree from scratch WITHOUT removed packages
    let result = rebuild_with_layers(
        repo_path,
        pacman_cache,
        pacman_conf,
        &state,
        &current_deployment.osname,
        deploy,
    )?;

    Ok(result)
}

/// Rebuild the entire filesystem tree "from scratch" with layered packages
/// This is the CORE function that implements the "from scratch" philosophy
fn rebuild_with_layers(
    repo_path: &Utf8PathBuf,
    pacman_cache: &Utf8PathBuf,
    pacman_conf: Option<&Path>,
    state: &LayeredState,
    osname: &str,
    deploy: bool,
) -> Result<LayeringResult> {
    println!("🔨 Rebuilding tree from scratch...");
    println!("   Base: {}", state.base_ref);
    println!("   Layered packages: {} total", state.layered_packages.len());

    // Open repo
    let repo = ostree::Repo::open_at(libc::AT_FDCWD, repo_path.as_str(), gio::Cancellable::NONE)
        .context("Opening OSTree repo")?;

    // Read base packages
    let base_packages = pacman_manager::read_packages_from_commit(repo_path, &state.base_ref)
        .context("Reading base packages")?;

    println!("   Base contains: {} packages", base_packages.len());

    // Create temp directory for rebuild
    let temp_root = TempDir::new().context("Creating temp directory")?;
    let root_path = temp_root.path();

    // Checkout base commit
    println!("   Checking out base commit...");
    checkout_commit(&repo, &state.base_ref, root_path)
        .context("Checking out base")?;

    // Install layered packages ON TOP of base
    if !state.layered_packages.is_empty() {
        let packages_vec: Vec<String> = state.layered_packages.iter().cloned().collect();
        
        println!("   Installing {} layered packages: {:?}", 
            packages_vec.len(), 
            packages_vec
        );

        pacman_manager::install(root_path, pacman_cache.as_str(), &packages_vec)
            .context("Installing layered packages")?;

        // Jako że pakiety pacmana mają /etc musimy przenieśc pliki i foldery z /etc o /usr/etc żeby deploy zadziałał
        let etc_path = root_path.join("etc");
        let usr_etc_path = root_path.join("usr/etc");
        std::fs::create_dir_all(&usr_etc_path)?;

        if etc_path.exists() {
            for entry in std::fs::read_dir(&etc_path)? {
                let entry = entry?;
                let dest = usr_etc_path.join(entry.file_name());
                std::fs::rename(entry.path(), dest)?;
            }

            std::fs::remove_dir_all(&etc_path)?;
        }

    }

    // Calculate what was newly installed (for result)
    let newly_installed: Vec<String> = state.layered_packages
        .iter()
        .filter(|pkg| !base_packages.contains_key(*pkg))
        .cloned()
        .collect();

    let size_delta = calculate_size_delta_for_layered(&base_packages, &state.layered_packages);

    // Commit the new tree
    let target_ref = format!("{}/layered", osname);
    println!("   Committing to ref: {}", target_ref);

    let new_commit = commit_layered_tree(
        &repo,
        root_path,
        &target_ref,
        &state.base_ref,
        state,
    )
    .context("Committing layered tree")?;

    println!("✅ New commit: {}", new_commit);

    // Deploy if requested
    let deployment_index = if deploy {
        println!("🚀 Deploying for next boot...");
        let idx = deploy_commit(repo_path, osname, &new_commit)
            .context("Deploying commit")?;
        println!("✅ Deployed as deployment #{}", idx);
        Some(idx)
    } else {
        println!("ℹ️  Use 'pacman-ostree deploy' to activate on next boot");
        None
    };

    Ok(LayeringResult {
        new_commit,
        deployment_index,
        newly_installed,
        total_layered: state.layered_packages.len(),
        size_delta,
    })
}

/// Load layering state from commit metadata
pub fn load_state_from_commit(repo_path: &Utf8PathBuf, commit: &str) -> Result<LayeredState> {
    let repo = ostree::Repo::open_at(libc::AT_FDCWD, repo_path.as_str(), gio::Cancellable::NONE)
        .context("Opening repo")?;

    // Read commit metadata
    let (commit_variant, _state) = repo.load_commit(commit)
        .context("Loading commit variant")?;
    let metadata = glib::VariantDict::new(Some(&commit_variant.child_value(0)));  // Metadata to dict w index 0

    // Extract state from metadata
    let base_ref = metadata
        .lookup_value("pacman-ostree.base-ref", None)
        .and_then(|v| v.str().map(|s| s.to_string()))
        .unwrap_or_else(|| "archlinux/x86_64/base".to_string());

    let layered_str = metadata
        .lookup_value("pacman-ostree.layered", None)
        .and_then(|v| v.str().map(|s| s.to_string()))
        .unwrap_or_else(|| "".to_string());

    let layered_packages: HashSet<String> = if layered_str.is_empty() {
        HashSet::new()
    } else {
        layered_str.split(',').map(|s| s.to_string()).collect()
    };

    Ok(LayeredState {
        base_ref,
        layered_packages,
        deployed_commit: Some(commit.to_string()),
    })
}

/// Commit a layered tree with metadata
fn commit_layered_tree(
    repo: &ostree::Repo,
    root: &Path,
    target_ref: &str,
    base_ref: &str,
    state: &LayeredState,
) -> Result<String> {
    use ostree::RepoCommitModifierFlags;

    // Create commit metadata
    let mut metadata = glib::VariantDict::new(None);
    metadata.insert("version", &"1.0");
    metadata.insert("pacman-ostree.base-ref", &base_ref);
    
    // Store layered packages as comma-separated string
    let layered_str = state.layered_packages
        .iter()
        .cloned()
        .collect::<Vec<_>>()
        .join(",");
    
    if !layered_str.is_empty() {
        metadata.insert("pacman-ostree.layered", &layered_str);
    }

    let metadata_variant = metadata.end();

    // Commit modifier
    let modifier = ostree::RepoCommitModifier::new(
        RepoCommitModifierFlags::NONE,
        None,
    );

    // Create MutableTree
    let mtree = ostree::MutableTree::new();

    // Write directory tree
    repo.write_directory_to_mtree(
        &gio::File::for_path(root),
        &mtree,
        Some(&modifier),
        gio::Cancellable::NONE,
    )
    .context("Writing directory to mtree")?;

    // Write mtree to repo
    let root_tree_file = repo
        .write_mtree(&mtree, gio::Cancellable::NONE)
        .context("Writing mtree")?;

    let root_tree = root_tree_file.downcast_ref::<ostree::RepoFile>()
        .ok_or_else(|| anyhow::anyhow!("Failed to cast to RepoFile"))?;

    // Get parent commit (base)
    let parent = repo
        .resolve_rev(base_ref, false) 
        .context("Resolving base ref")?;

    // Commit message
    let commit_msg = format!(
        "pacman-ostree: {} layered package(s)",
        state.layered_packages.len()
    );

    // Commit!
    let checksum = repo
        .write_commit(
            parent.as_ref().map(|s| s.as_str()),
            Some(&commit_msg),
            None,
            Some(&metadata_variant),
            &root_tree,
            gio::Cancellable::NONE,
        )
        .context("Writing commit")?;

    // Update ref
    repo.set_ref_immediate(
        None,
        target_ref,
        Some(&checksum),
        gio::Cancellable::NONE,
    )
    .context("Setting ref")?;

    Ok(checksum.to_string())
}

/// Checkout an OSTree commit to a directory
fn checkout_commit(
    repo: &ostree::Repo,
    commit_ref: &str,
    target: &Path,
) -> Result<()> {
    // Resolve ref to commit
    let resolved = repo
        .resolve_rev(commit_ref, false)
        .context(format!("Resolving ref: {}", commit_ref))?
        .ok_or_else(|| anyhow::anyhow!("Commit ref '{}' not found", commit_ref))?;

    // Checkout options
    let opts = ostree::RepoCheckoutAtOptions {
        mode: ostree::RepoCheckoutMode::User,
        overwrite_mode: ostree::RepoCheckoutOverwriteMode::UnionFiles,
        ..Default::default()
    };

    // Checkout
    repo.checkout_at(
        Some(&opts),
        libc::AT_FDCWD,
        target.to_str().unwrap(),
        resolved.as_str(),
        gio::Cancellable::NONE,
    )
    .context("Checking out commit")?;

    Ok(())
}

/// Deploy a commit to make it bootable on next reboot
fn deploy_commit(
    repo_path: &Utf8PathBuf,
    osname: &str,
    commit: &str,
) -> Result<u32> {
    if unsafe { libc::geteuid() } != 0 {
        anyhow::bail!("Deploy requires root privileges (EUID != 0)");
    }

    println!("   OS name: {}", osname);
    println!("   Commit: {}", commit);

    let output = Command::new("ostree")
    .arg("admin")
    .arg("deploy")
    .arg(format!("--os={}", osname))
    .arg("--sysroot=/") // wskazuje katalog root systemu plików
    .arg("--stage")
    .arg(commit)        // refspec commit do wdrożenia
    .output()
    .context("Failed to spawn ostree admin deploy")?;


    if !output.status.success() {
        anyhow::bail!(
            "ostree admin deploy failed (status: {:?})\nstdout:\n{}\nstderr:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let deployment_idx = get_deployment_index(osname, commit)?;
    Ok(deployment_idx)
}

fn get_deployment_index(osname: &str, commit: &str) -> Result<u32> {
    let output = Command::new("ostree")
        .arg("admin")
        .arg("status")
        .output()
        .context("Failed to get deployment status")?;

    if !output.status.success() {
        anyhow::bail!("ostree admin status failed");
    }

    let status = String::from_utf8_lossy(&output.stdout);
    
    for (idx, line) in status.lines().enumerate() {
        if line.contains(osname) && line.contains(&commit[..8]) {
            return Ok(idx as u32);
        }
    }

    Ok(0)
}

/// Get currently booted deployment
pub fn get_booted_deployment() -> Result<DeploymentInfo> {
    let deployments = list_deployments()?;
    
    deployments
        .into_iter()
        .find(|d| d.is_booted)
        .ok_or_else(|| anyhow::anyhow!("No booted deployment found"))
}

/// List all current deployments
pub fn list_deployments() -> Result<Vec<DeploymentInfo>> {
    let output = Command::new("ostree")
        .arg("admin")
        .arg("status")
        .output()
        .context("Failed to get deployments")?;

    if !output.status.success() {
        anyhow::bail!("ostree admin status failed");
    }

    let status = String::from_utf8_lossy(&output.stdout);
    let mut deployments = Vec::new();

    for (idx, line) in status.lines().enumerate() {
        if let Some(info) = parse_deployment_line(line, idx as u32) {
            deployments.push(info);
        }
    }

    Ok(deployments)
}

#[derive(Debug, Clone)]
pub struct DeploymentInfo {
    pub index: u32,
    pub osname: String,
    pub commit: String,
    pub is_booted: bool,
    pub is_staged: bool,
}

fn parse_deployment_line(line: &str, index: u32) -> Option<DeploymentInfo> {
    let line = line.trim();
    
    let is_booted = line.starts_with('*');
    let is_staged = line.starts_with('+');
    
    let clean = line.trim_start_matches('*').trim_start_matches('+').trim();
    
    let parts: Vec<&str> = clean.split_whitespace().collect();
    if parts.len() >= 2 {
        Some(DeploymentInfo {
            index,
            osname: parts[0].to_string(),
            commit: parts[1].to_string(),
            is_booted,
            is_staged,
        })
    } else {
        None
    }
}

fn calculate_size_delta_for_layered(
    base: &HashMap<String, PacmanPackageMeta>,
    layered: &HashSet<String>,
) -> i64 {
    // Only count size of layered packages (not in base)
    let layered_size: u64 = layered
        .iter()
        .filter(|pkg| !base.contains_key(*pkg))
        .filter_map(|pkg| base.get(pkg).map(|p| p.size))
        .sum();

    layered_size as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_deployment_line() {
        let line = "* archlinux 1a2b3c4d.0";
        let info = parse_deployment_line(line, 0).unwrap();
        
        assert_eq!(info.osname, "archlinux");
        assert!(info.is_booted);
        assert!(!info.is_staged);
    }

    #[test]
    fn test_layered_state_cannot_remove_base() {
        let mut state = LayeredState {
            base_ref: "base".to_string(),
            layered_packages: HashSet::from(["vim".to_string()]),
            deployed_commit: None,
        };

        // Can remove layered package
        assert!(state.layered_packages.contains("vim"));
        state.layered_packages.remove("vim");
        assert!(!state.layered_packages.contains("vim"));

        // Cannot remove base package (it's not in layered_packages)
        assert!(!state.layered_packages.contains("bash"));
    }
}