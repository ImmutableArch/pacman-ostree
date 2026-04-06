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

    let dest_db_path = format!("{}/var/lib/pacman/local", dest);
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
        }
    }

    println!("Running PostTransaction hooks...");
    run_hooks(&hooks, HookWhen::PostTransaction, install_result, dest, &installed_files)?;

    for package_info in &install_result.packages {
        write_package_to_database(package_info, dest).await.ok();
    }

    Ok(())
}

// ───────────────── helpers ─────────────────

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

    let mut cmd = build_bwrap_base(dest);
    cmd.arg("--bind").arg(&script_path).arg("/run/script.sh")
        .arg("--")
        .arg("/bin/bash").arg("/run/script.sh");

    let status = cmd.status()?;

    let _ = std::fs::remove_file(&script_path);

    if !status.success() {
        anyhow::bail!("Script failed: {} ({:?})", pkg_name, status.code());
    }

    Ok(())
}

fn run_hook_sandboxed(
    hook: &PacmanHook,
    dest: &str,
    matched_targets: &[String],
) -> anyhow::Result<()> {
    let parts: Vec<&str> = hook.action.exec.split_whitespace().collect();

    if parts.is_empty() {
        anyhow::bail!("Empty hook exec: {}", hook.name);
    }

    let mut cmd = build_bwrap_base(dest);
    cmd.arg("--");
    cmd.args(&parts);

    if hook.action.needs_targets {
        cmd.stdin(Stdio::piped());
    }

    let mut child = cmd.spawn()?;

    if hook.action.needs_targets {
        if let Some(mut stdin) = child.stdin.take() {
            for target in matched_targets {
                writeln!(stdin, "{}", target)?;
            }
        }
    }

    let status = child.wait()?;

    if !status.success() {
        anyhow::bail!("Hook failed: {} ({:?})", hook.name, status.code());
    }

    Ok(())
}

pub async fn write_package_to_database(
    pkg_info: &PackageInfo,
    dest: &str,
) -> anyhow::Result<()> {
    let pkg = &pkg_info.package;
    let entry_name = format!("{}-{}-{}", pkg.name, pkg.version, pkg.pkgrel);

    let db_dir = format!("{}/var/lib/pacman/local/{}", dest, entry_name);
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
    let local_db = format!("{}/var/lib/pacman/local", dest);
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

fn build_bwrap_base(dest: &str) -> Command {
    let mut cmd = Command::new("bwrap");

    let mut cmd = Command::new("bwrap");
    cmd
        .arg("--bind").arg(dest).arg("/")
        .arg("--proc").arg("/proc")
        .arg("--dev").arg("/dev")
        .arg("--ro-bind").arg("/sys").arg("/sys")
        .arg("--unshare-net")
        .arg("--unshare-ipc")
        .arg("--cap-drop").arg("ALL")
        .arg("--cap-add").arg("cap_chown")
        .arg("--cap-add").arg("cap_setgid")
        .arg("--cap-add").arg("cap_setuid")
        .arg("--cap-add").arg("cap_dac_override")
        .arg("--cap-add").arg("cap_fowner")
        .arg("--tmpfs").arg("/tmp")
        .arg("--tmpfs").arg("/run")
        .arg("--chdir").arg("/")
        .arg("--setenv").arg("DBUS_SESSION_BUS_ADDRESS").arg("disabled:")
        .arg("--setenv").arg("SYSTEMD_OFFLINE").arg("1")
        .arg("--setenv").arg("container").arg("systemd-nspawn");
    cmd
}