use clap::{Parser, Subcommand};
use std::error::Error;
use anyhow::{Context, Result};
use std::env;
use camino::Utf8PathBuf;

mod compose;
mod pacman_manager;
mod container;
mod layered_packages;

#[derive(Parser, Debug)]
#[command(author, version, about = "A program that connects pacman with ostree")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Compose Arch-based OSTree OCI image
    Compose(compose::ComposeImageOpts),
    
    /// Install packages on top of base system
    Install {
        /// Packages to install
        #[arg(required = true)]
        packages: Vec<String>,
        
        /// OSTree repository path
        #[arg(long, default_value = "/ostree/repo")]
        repo: String,
        
        /// Pacman cache directory
        #[arg(long, default_value = "/var/cache/pacman/pkg")]
        cache: String,
        
        /// Pacman config file
        #[arg(long)]
        config: Option<String>,
        
        /// Skip deployment (only create commit, don't deploy)
        #[arg(long)]
        no_deploy: bool,
    },
    
    /// Remove layered packages
    Remove {
        /// Packages to remove (only layered packages, not base)
        #[arg(required = true)]
        packages: Vec<String>,
        
        /// OSTree repository path
        #[arg(long, default_value = "/ostree/repo")]
        repo: String,
        
        /// Pacman cache directory
        #[arg(long, default_value = "/var/cache/pacman/pkg")]
        cache: String,
        
        /// Pacman config file
        #[arg(long)]
        config: Option<String>,
        
        /// Skip deployment (only create commit, don't deploy)
        #[arg(long)]
        no_deploy: bool,
    },
    
    /// Show status of layered packages
    Status {
        /// OSTree repository path
        #[arg(long, default_value = "/ostree/repo")]
        repo: String,
    },
    
    /// Reset to base system (remove all layered packages)
    Reset {
        /// OSTree repository path
        #[arg(long, default_value = "/ostree/repo")]
        repo: String,
        
        /// Skip deployment (only create commit, don't deploy)
        #[arg(long)]
        no_deploy: bool,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    
    match cli.command {
        Commands::Compose(opts) => {
            println!("Running compose with config: {:?}", opts.manifest);
            let config = compose::yaml_parse(opts.manifest.as_str())?;
            compose::run(&config, &opts).await;
        }
        
        Commands::Install {
            packages,
            repo,
            cache,
            config,
            no_deploy,
        } => {
            handle_install(packages, repo, cache, config, !no_deploy)?;
        }
        
        Commands::Remove {
            packages,
            repo,
            cache,
            config,
            no_deploy,
        } => {
            handle_remove(packages, repo, cache, config, !no_deploy)?;
        }
        
        Commands::Status { repo } => {
            handle_status(repo)?;
        }
        
        Commands::Reset { repo, no_deploy } => {
            handle_reset(repo, !no_deploy)?;
        }
    }
    
    Ok(())
}

/// Handle package installation
fn handle_install(
    packages: Vec<String>,
    repo: String,
    cache: String,
    config: Option<String>,
    deploy: bool,
) -> Result<()> {
    // Check if running as root
    if unsafe { libc::geteuid() } != 0 {
        anyhow::bail!("Package installation requires root privileges");
    }
    
    let repo_path = Utf8PathBuf::from(repo);
    let cache_path = Utf8PathBuf::from(cache);
    let config_path = config.as_ref().map(|s| std::path::PathBuf::from(s));
    
    // Try to get current deployment
    let deployment_result = layered_packages::get_booted_deployment();
    
    match deployment_result {
        Ok(deployment) => {
            // Normal case: system is already booted with a deployment
            println!("📦 Installing packages on current deployment");
            let result = layered_packages::install_packages(
                &repo_path,
                &cache_path,
                config_path.as_deref(),
                &packages,
                deploy,
            )?;
            
            print_install_result(&result, deploy);
        }
        Err(_) => {
            // Fresh system: auto-detect base ref from origin
            println!("🆕 No booted deployment detected - initializing from origin");
            
            let base_ref = detect_base_ref(&repo_path)
                .context("Could not auto-detect base ref. Is the system properly deployed?")?;
            
            println!("   Auto-detected base ref: {}", base_ref);
            
            let result = layered_packages::install_packages_fresh(
                &repo_path,
                &cache_path,
                config_path.as_deref(),
                &base_ref,
                &packages,
                deploy,
            )?;
            
            print_install_result(&result, deploy);
        }
    }
    
    Ok(())
}

/// Auto-detect base ref from sysroot origin
fn detect_base_ref(repo_path: &Utf8PathBuf) -> Result<String> {
    use std::fs;
    use std::path::PathBuf;
    
    // Try to read origin from deployed system
    // Usually in /ostree/deploy/<osname>/deploy/<commit>.origin
    let deploy_dir = PathBuf::from("/ostree/deploy");
    
    if !deploy_dir.exists() {
        anyhow::bail!("Not running on an OSTree system (no /ostree/deploy)");
    }
    
    // Find first OS deployment
    for entry in fs::read_dir(&deploy_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        
        let osname = entry.file_name();
        let deploy_subdir = entry.path().join("deploy");
        
        if !deploy_subdir.exists() {
            continue;
        }
        
        // Find .origin file
        for deploy_entry in fs::read_dir(&deploy_subdir)? {
            let deploy_entry = deploy_entry?;
            let path = deploy_entry.path();
            
            if let Some(ext) = path.extension() {
                if ext == "origin" {
                    // Read origin file
                    let content = fs::read_to_string(&path)
                        .context("Reading origin file")?;
                    
                    // Parse refspec= line
                    for line in content.lines() {
                        if line.starts_with("refspec=") {
                            let refspec = line.strip_prefix("refspec=").unwrap();
                            // Remove remote prefix if present (e.g., "remote:ref" -> "ref")
                            let base_ref = refspec.split(':').last().unwrap_or(refspec);
                            return Ok(base_ref.to_string());
                        }
                    }
                }
            }
        }
    }
    
    anyhow::bail!(
        "Could not detect base ref from system. \
         Make sure the system is properly deployed with OSTree."
    )
}

/// Handle package removal
fn handle_remove(
    packages: Vec<String>,
    repo: String,
    cache: String,
    config: Option<String>,
    deploy: bool,
) -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        anyhow::bail!("Package removal requires root privileges");
    }
    
    let repo_path = Utf8PathBuf::from(repo);
    let cache_path = Utf8PathBuf::from(cache);
    let config_path = config.as_ref().map(|s| std::path::PathBuf::from(s));
    
    println!("🗑️  Removing packages: {:?}", packages);
    
    let result = layered_packages::remove_packages(
        &repo_path,
        &cache_path,
        config_path.as_deref(),
        &packages,
        deploy,
    )?;
    
    println!("\n✅ Package removal complete!");
    println!("   Removed: {} packages", packages.len());
    println!("   Remaining layered: {}", result.total_layered);
    println!("   New commit: {}", result.new_commit);
    
    if let Some(idx) = result.deployment_index {
        println!("   Deployed as: deployment #{}", idx);
        println!("\n🔄 Reboot to activate changes");
    } else {
        println!("\nℹ️  Commit created but not deployed. Use without --no-deploy to deploy.");
    }
    
    Ok(())
}

/// Handle status display
fn handle_status(repo: String) -> Result<()> {
    let repo_path = Utf8PathBuf::from(repo);
    
    println!("📊 pacman-ostree status\n");
    
    // Get current deployment
    match layered_packages::get_booted_deployment() {
        Ok(deployment) => {
            println!("Current deployment:");
            println!("  OS: {}", deployment.osname);
            println!("  Commit: {}", deployment.commit);
            println!();
            
            // Load layered state
            match layered_packages::load_state_from_commit(&repo_path, &deployment.commit) {
                Ok(state) => {
                    println!("Base ref: {}", state.base_ref);
                    println!();
                    
                    if state.layered_packages.is_empty() {
                        println!("No layered packages (using base system only)");
                    } else {
                        println!("Layered packages ({}):", state.layered_packages.len());
                        let mut packages: Vec<_> = state.layered_packages.iter().collect();
                        packages.sort();
                        for pkg in packages {
                            println!("  • {}", pkg);
                        }
                    }
                }
                Err(e) => {
                    println!("⚠️  Could not read layering state: {}", e);
                }
            }
        }
        Err(e) => {
            println!("⚠️  No booted deployment found: {}", e);
            println!("\nThis might be a fresh installation.");
            println!("Run: sudo pacman-ostree install <packages>");
            println!("(base ref will be auto-detected from deployed system)");
        }
    }
    
    // Show all deployments
    println!("\n─────────────────────────────────────");
    println!("All deployments:");
    match layered_packages::list_deployments() {
        Ok(deployments) => {
            if deployments.is_empty() {
                println!("  (none)");
            } else {
                for d in deployments {
                    let marker = if d.is_booted {
                        "* "
                    } else if d.is_staged {
                        "+ "
                    } else {
                        "  "
                    };
                    println!("{}{} {} {}", marker, d.index, d.osname, d.commit);
                }
                println!("\n  * = booted, + = staged");
            }
        }
        Err(e) => {
            println!("  Could not list deployments: {}", e);
        }
    }
    
    Ok(())
}

/// Handle reset (remove all layered packages)
fn handle_reset(repo: String, deploy: bool) -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        anyhow::bail!("Reset requires root privileges");
    }
    
    let repo_path = Utf8PathBuf::from(repo);
    
    println!("🔄 Resetting to base system (removing all layered packages)");
    
    let deployment = layered_packages::get_booted_deployment()
        .context("Getting current deployment")?;
    
    let state = layered_packages::load_state_from_commit(&repo_path, &deployment.commit)?;
    
    if state.layered_packages.is_empty() {
        println!("✅ Already at base system (no layered packages)");
        return Ok(());
    }
    
    let packages_to_remove: Vec<String> = state.layered_packages.iter().cloned().collect();
    println!("   Removing {} packages: {:?}", packages_to_remove.len(), packages_to_remove);
    
    let cache_path = Utf8PathBuf::from("/var/cache/pacman/pkg");
    let result = layered_packages::remove_packages(
        &repo_path,
        &cache_path,
        None,
        &packages_to_remove,
        deploy,
    )?;
    
    println!("\n✅ Reset complete! Back to base system.");
    println!("   New commit: {}", result.new_commit);
    
    if let Some(idx) = result.deployment_index {
        println!("   Deployed as: deployment #{}", idx);
        println!("\n🔄 Reboot to activate");
    } else {
        println!("\nℹ️  Commit created but not deployed. Run without --no-deploy to deploy.");
    }
    
    Ok(())
}

/// Print installation result
fn print_install_result(result: &layered_packages::LayeringResult, deployed: bool) {
    println!("\n✅ Package installation complete!");
    println!("   Newly installed: {} packages", result.newly_installed.len());
    if !result.newly_installed.is_empty() {
        for pkg in &result.newly_installed {
            println!("     • {}", pkg);
        }
    }
    println!("   Total layered: {}", result.total_layered);
    println!("   Size delta: {} bytes", result.size_delta);
    println!("   New commit: {}", result.new_commit);
    
    if let Some(idx) = result.deployment_index {
        println!("   Deployed as: deployment #{}", idx);
        println!("\n🔄 Reboot to activate changes");
    } else {
        println!("\nℹ️  Commit created but not deployed. Run without --no-deploy to deploy.");
    }
}