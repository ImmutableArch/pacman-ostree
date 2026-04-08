// Obsługa instalacji pakietów i odinstalowania

use crate::package_manager::{AlpmRepository, InstallResult, PackageManager, PackageInfo, PacmanHook, InstallReason};
use crate::package_manager::pacman_hooks::{HookWhen, hook_matches, load_hooks};
use crate::{AlpmPool, AlpmPackage};

use anyhow::Context;
use std::fs;
use std::io::Write;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use crate::bubblewrap::{Bubblewrap, BubblewrapMutability};
use std::os::unix::fs::FileTypeExt;
use walkdir::WalkDir;

use indicatif::ProgressStyle;
use console::style;

#[derive(Debug)]
struct PackageScripts {
    pkg_name:       String,
    script_content: String,
    has_pre:        bool,
    has_post:       bool,
}

const DEFAULT_PACMAN_CONF_PATH: &str = "/etc/pacman.conf";

pub async fn install_packages(package_names: Vec<&str>, dest: &str, pacman_conf: Option<&str>) -> anyhow::Result<()> {
    install_packages_with_cache(package_names, dest, pacman_conf, None).await
}

pub async fn install_packages_with_cache(
    package_names: Vec<&str>,
    dest: &str,
    pacman_conf: Option<&str>,
    cache_dir: Option<&str>,
) -> anyhow::Result<()> {
    let pacman_conf = pacman_conf.unwrap_or(DEFAULT_PACMAN_CONF_PATH);
    let default_cache = format!("{}/var/cache/pacman/pkg", dest);
    let cache_dir = cache_dir.unwrap_or(&default_cache);

    if !Path::new(&format!("{}/usr/share/pacman", dest)).exists() {
        fs::create_dir_all(format!("{}/usr/share/pacman", dest))?;
        fs::create_dir_all(format!("{}/usr/share/pacman/sync", dest))?;
        fs::create_dir_all(format!("{}/usr/share/pacman/local", dest))?;
    }

    fs::create_dir_all(cache_dir)?;

    let install_result = resolve_package_install(package_names, pacman_conf, dest).await?;
    download_packages(&install_result, dest, cache_dir, pacman_conf).await?;
    unpack_packages(&install_result, dest).await?;

    Ok(())
}

async fn resolve_package_install(
    package_names: Vec<&str>,
    _pacman_conf: &str,
    dest: &str,
) -> anyhow::Result<InstallResult> {
    let repo = AlpmRepository::new()
        .map_err(|e| anyhow::anyhow!("Failed to initialize ALPM: {}", e))?;

    println!("Resolving packages...");

    let expanded = repo.expand_names(package_names.to_vec())
        .unwrap_or_else(|_| package_names.iter().map(|s| s.to_string()).collect());

    let expanded_names: Vec<&str> = expanded.iter().map(|s| s.as_str()).collect();

    let mut pool = repo.load_sync_to_pool()
        .map_err(|e| anyhow::anyhow!("Failed to load repository: {}", e))?;

    let dest_db_path = format!("{}/usr/share/pacman/local", dest);
    if Path::new(&dest_db_path).exists() {
        if let Ok(local_packages) = load_local_db_from_files(dest) {
            let mut local_pool = AlpmPool::new();
            for pkg in local_packages {
                local_pool.add_package(pkg);
            }
            local_pool.finalize_virtuals();
            pool.merge_local(local_pool);
        }
    }

    let pm = PackageManager::new(pool);
    let install_result = pm.plan_install(expanded_names).await
        .map_err(|e| anyhow::anyhow!("Dependency resolution failed: {}", e))?;

    Ok(install_result)
}

async fn download_packages(
    install_result: &InstallResult,
    _dest: &str,
    cache_dir: &str,
    _pacman_conf: &str,
) -> anyhow::Result<()> {
    if install_result.packages.is_empty() {
        return Ok(());
    }

    let mut repo = AlpmRepository::new()?;
    fs::create_dir_all(cache_dir)?;

    let pkg_names: Vec<String> = install_result.packages
        .iter()
        .map(|p| p.package.name.clone())
        .collect();

    println!("Downloading {} packages...", pkg_names.len());

    repo.download_packages_to_cache(&pkg_names, cache_dir)
        .map_err(|e| anyhow::anyhow!("Download failed: {}", e))?;

    Ok(())
}

pub async fn unpack_packages(install_result: &InstallResult, dest: &str) -> anyhow::Result<()> {
    let cache_dir = format!("{}/var/cache/pacman/pkg", dest);

    for package_info in &install_result.packages {
        let pkg_name = &package_info.package.name;

        let pattern  = format!("{}/{}-*.pkg.tar.zst", cache_dir, pkg_name);
        let pkg_file = match glob::glob(&pattern)?.filter_map(Result::ok).next() {
            Some(f) => f,
            None => continue,
        };

        let status = Command::new("tar")
            .arg("--extract")
            .arg("--zstd")
            .arg("--xattrs-include=*.*")
            .arg("--numeric-owner")
            .arg("--preserve-permissions")
            .arg("--file").arg(&pkg_file)
            .arg("--directory").arg(dest)
            .arg("--exclude=.PKGINFO")
            .arg("--exclude=.BUILDINFO")
            .arg("--exclude=.MTREE")
            .arg("--exclude=.INSTALL")
            .arg("--exclude=.CHANGELOG")
            .status()?;

        if !status.success() {
            anyhow::bail!("tar failed for {}: {:?}", pkg_name, status.code());
        }
    }

    let hooks = load_hooks(dest)?;
    let installed_files = collect_installed_files(install_result, &cache_dir)?;

    let mut all_scripts: Vec<PackageScripts> = Vec::new();

    for package_info in &install_result.packages {
        let pkg_name = &package_info.package.name;

        let pattern  = format!("{}/{}-*.pkg.tar.zst", cache_dir, pkg_name);
        let pkg_file = match glob::glob(&pattern)?.filter_map(Result::ok).next() {
            Some(f) => f,
            None => {
                all_scripts.push(PackageScripts {
                    pkg_name: pkg_name.clone(),
                    script_content: String::new(),
                    has_pre: false,
                    has_post: false,
                });
                continue;
            }
        };

        let scripts = if let Some(content) = extract_install_script(&pkg_file)? {
            let has_pre  = content.contains("pre_install()");
            let has_post = content.contains("post_install()");

            if has_pre {
                println!("Running pre_install for {}...", pkg_name);
                run_install_script_sandboxed(&content, dest, pkg_name, "pre_install")?;
            }

            PackageScripts {
                pkg_name: pkg_name.clone(),
                script_content: content,
                has_pre,
                has_post,
            }
        } else {
            PackageScripts {
                pkg_name: pkg_name.clone(),
                script_content: String::new(),
                has_pre: false,
                has_post: false,
            }
        };

        all_scripts.push(scripts);
    }

    println!("Running PreTransaction hooks...");
    run_hooks(&hooks, HookWhen::PreTransaction, install_result, dest, &installed_files)?;

    for scripts in &all_scripts {
        if scripts.has_post {
            run_install_script_sandboxed(
                &scripts.script_content,
                dest,
                &scripts.pkg_name,
                "post_install",
            )?;
            println!("Ran post_install for {}", scripts.pkg_name);
        }
    }

    println!("Running PostTransaction hooks...");
    run_hooks(&hooks, HookWhen::PostTransaction, install_result, dest, &installed_files)?;

    for package_info in &install_result.packages {
        write_package_to_database(package_info, dest).await.ok();
    }

    cleanup_special_files(dest)?;

    Ok(())
}

// ───────────────── helpers ─────────────────

fn cleanup_special_files(root: &str) -> anyhow::Result<()> {
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        let path = entry.path();

        let metadata = fs::symlink_metadata(path)?;
        let file_type = metadata.file_type();

        if file_type.is_socket()
            || file_type.is_fifo()
            || file_type.is_block_device()
            || file_type.is_char_device()
        {
            println!("Removing special file: {}", path.display());
            fs::remove_file(path)?;
        }
    }

    Ok(())
}

fn extract_install_script(pkg_file: &Path) -> anyhow::Result<Option<String>> {
    let output = Command::new("tar")
        .arg("--extract")
        .arg("--zstd")
        .arg("--to-stdout")
        .arg("--file").arg(pkg_file)
        .arg(".INSTALL")
        .output()?;

    if output.status.success() && !output.stdout.is_empty() {
        Ok(Some(String::from_utf8(output.stdout)?))
    } else {
        Ok(None)
    }
}

fn run_hooks(
    hooks: &[PacmanHook],
    when: HookWhen,
    install_result: &InstallResult,
    dest: &str,
    installed_files: &[String],
) -> anyhow::Result<()> {
    let installed_packages: Vec<String> = install_result.packages
        .iter()
        .map(|p| p.package.name.clone())
        .collect();

    for hook in hooks.iter().filter(|h| h.action.when == when) {
        let matched = hook_matches(hook, &installed_packages, installed_files);
        if !matched.is_empty() {
            println!("Running hook {}...", hook.name);
            run_hook_sandboxed(hook, dest, &matched)?;
        }
    }

    Ok(())
}

fn collect_installed_files(
    install_result: &InstallResult,
    cache_dir: &str
) -> anyhow::Result<Vec<String>> {
    let mut all_files = Vec::new();

    for package_info in &install_result.packages {
        let pkg_name = &package_info.package.name;
        let pattern  = format!("{}/{}-*.pkg.tar.zst", cache_dir, pkg_name);

        if let Some(pkg_file) = glob::glob(&pattern)?.filter_map(Result::ok).next() {
            let output = Command::new("tar")
                .arg("--list")
                .arg("--zstd")
                .arg("--file").arg(&pkg_file)
                .output()?;

            if output.status.success() {
                for line in String::from_utf8_lossy(&output.stdout).lines() {
                    let line = line.trim_start_matches('.');
                    if !line.starts_with("/.") && !line.is_empty() {
                        all_files.push(line.to_string());
                    }
                }
            }
        }
    }

    all_files.dedup();
    Ok(all_files)
}

pub async fn write_package_to_database(
    pkg_info: &PackageInfo,
    dest: &str,
) -> anyhow::Result<()> {
    println!("Writing Pacman Databse...");
    let pkg = &pkg_info.package;
    let entry_name = format!("{}-{}-{}", pkg.name, pkg.version, pkg.pkgrel);

    let db_dir = format!("{}/usr/share/pacman/local/{}", dest, entry_name);
    std::fs::create_dir_all(&db_dir)?;

    std::fs::write(
        format!("{}/desc", db_dir),
        format!("%NAME%\n{}\n\n%VERSION%\n{}-{}\n\n",
            pkg.name, pkg.version, pkg.pkgrel)
    )?;

    std::fs::write(
        format!("{}/files", db_dir),
        "%FILES%\n\n"
    )?;

    Ok(())
}

pub fn load_local_db_from_files(dest: &str) -> anyhow::Result<Vec<AlpmPackage>> {
    let local_db = format!("{}/usr/share/pacman/local", dest);
    let mut packages = Vec::new();

    let entries = match std::fs::read_dir(&local_db) {
        Ok(e) => e,
        Err(_) => return Ok(packages),
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        if !path.is_dir() {
            continue;
        }

        let desc_path = path.join("desc");
        if !desc_path.exists() {
            continue;
        }

        let content = std::fs::read_to_string(&desc_path)?;

        let mut name = String::new();
        let mut version = String::new();

        let mut current = "";

        for line in content.lines() {
            if line == "%NAME%" {
                current = "name";
            } else if line == "%VERSION%" {
                current = "version";
            } else if !line.is_empty() {
                match current {
                    "name" => name = line.to_string(),
                    "version" => version = line.to_string(),
                    _ => {}
                }
            }
        }

        if name.is_empty() || version.is_empty() {
            continue;
        }

        let (ver, pkgrel) = match version.rfind('-') {
            Some(pos) => (version[..pos].to_string(), version[pos+1..].to_string()),
            None => (version.clone(), "1".to_string()),
        };

        packages.push(AlpmPackage {
            name,
            version: ver,
            pkgrel,
            repo: "local".to_string(),
            size: 0,
            deps: vec![],
            provides: vec![],
            conflicts: vec![],
        });
    }

    Ok(packages)
}

fn build_bwrap_base(dest: &str) -> anyhow::Result<Bubblewrap> {
    let mut bwrap = Bubblewrap::new(dest)?;
    bwrap.prepend_rootfs_bind(dest, "/");

    // dodatkowe bindy z oryginalnego Command
    bwrap.bind_read("/sys", "/sys");

    // dodatkowe katalogi tmp
    bwrap.bind_readwrite("/tmp", "/tmp");
    bwrap.bind_readwrite("/run", "/run");

    // zmienne środowiskowe
    bwrap.setenv("DBUS_SESSION_BUS_ADDRESS", "disabled:");
    bwrap.setenv("SYSTEMD_OFFLINE", "1");
    bwrap.setenv("container", "systemd-nspawn");

    Ok(bwrap)
}

// === Run install script sandboxed ===
fn run_install_script_sandboxed(
    script_content: &str,
    dest: &str,
    pkg_name: &str,
    function_name: &str,
) -> anyhow::Result<()> {
    let script_path = format!("/tmp/.install-{}-{}.sh", pkg_name, function_name);
    let full_script = format!(
        "#!/bin/bash\nset -e\n\n{}\n\n{}\n",
        script_content, function_name
    );
    std::fs::write(&script_path, &full_script)?;

    let mut bwrap = build_bwrap_base(dest)?;
    bwrap.bind_read(&script_path, "/run/script.sh");
    bwrap.append_child_argv(["/bin/bash", "/run/script.sh"]);

    let status = bwrap.run()?;

    let _ = std::fs::remove_file(&script_path);

    Ok(())
}

// === Run hook sandboxed ===
fn run_hook_sandboxed(
    hook: &PacmanHook,
    dest: &str,
    matched_targets: &[String],
) -> anyhow::Result<()> {
    let parts: Vec<&str> = hook.action.exec.split_whitespace().collect();
    if parts.is_empty() {
        anyhow::bail!("Empty hook exec: {}", hook.name);
    }

    let mut bwrap = build_bwrap_base(dest)?;
    bwrap.append_child_argv(parts.iter().copied());

    let result = if hook.action.needs_targets && !matched_targets.is_empty() {
        let input = (matched_targets.join("\n") + "\n").into_bytes();
        bwrap.run_with_stdin(&input)
    } else {
        bwrap.run()
    };

    // Nie failuj na błędach hooków — loguj ostrzeżenie
    if let Err(e) = result {
        eprintln!("  Warning hook {}, ended with error: {}", hook.name, e);
    }

    Ok(())
}