//Things to do after installing packages from compose

use crate::compose::ConfigYaml;
use crate::initramfs::run_dracut;
use crate::bubblewrap::Bubblewrap;
use anyhow::Result;
use cap_std::fs::Dir;
use std::fs;
use std::path::Path;
use std::io::{self, BufRead, Write};
use camino::Utf8Path;
use std::process::Command;
use ostree_ext::bootabletree::find_kernel_dir_fs;
use std::os::unix::fs::PermissionsExt;
use cap_std::fs::Permissions;
use std::fs::Permissions as StdPermissions;
use anyhow::Context;

fn prepare_rootfs(root_fs: &Dir) -> Result<()> {
    println!("Preparing root filesystem...");
    let prepare_conf = "[composefs]\nenabled = yes\n[sysroot]\nreadonly = true\n";
    root_fs.write("usr/lib/ostree/prepare-root.conf", prepare_conf.as_bytes())
        .context("Failed to write prepare-root.conf")?;

    setup_base_dirs(root_fs)?;

    Ok(())
}

fn setup_base_dirs(root_fs: &Dir) -> Result<()> {
    // 1. Usuń stare katalogi
    let remove_dirs = ["boot", "home", "root", "usr/local", "srv", "opt", "mnt", "var"];
    for dir in remove_dirs {
        let _ = root_fs.remove_dir_all(dir);
    }

    // 2. Utwórz podstawowe katalogi
    let create_dirs = ["sysroot", "boot", "usr/lib/ostree", "var"];
    for dir in create_dirs {
        root_fs.create_dir_all(dir)
            .with_context(|| format!("Creating directory {}", dir))?;
    }

    // 3. Utwórz symlinki
    let symlinks = [
        ("sysroot/ostree", "ostree"),
        ("var/roothome", "root"),
        ("var/srv", "srv"),
        ("var/opt", "opt"),
        ("var/mnt", "mnt"),
        ("var/home", "home"),
        ("../var/usrlocal", "usr/local"),
        ("usr/share/pacman", "var/lib/pacman"),
    ];
    for (src, dst) in symlinks {
        ensure_parent_exists(root_fs, dst)?;
        let _ = root_fs.remove_file(dst);
        let _ = root_fs.remove_dir_all(dst);
        root_fs.symlink(src, dst)?;
    }
    // 4. Utwórz struktury w /var z odpowiednimi uprawnieniami
    let var_dirs_0755 = ["opt", "home", "srv", "mnt", "usrlocal"];
    for d in &var_dirs_0755 {
        let path = format!("var/{}", d);
        root_fs.create_dir_all(&path)?;
        root_fs.set_permissions(&path, Permissions::from_std(StdPermissions::from_mode(0o755)))?;
    }

    // specjalne katalogi
    root_fs.create_dir_all("var/roothome")?;
    root_fs.set_permissions("var/roothome", Permissions::from_std(StdPermissions::from_mode(0o700)))?;

    root_fs.create_dir_all("run/media")?;
    root_fs.set_permissions("run/media", Permissions::from_std(StdPermissions::from_mode(0o755)))?;

    Ok(())
}

fn ensure_parent_exists(root_fs: &Dir, path: &str) -> Result<()> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        root_fs.create_dir_all(parent)?;
    }
    Ok(())
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

fn execute_post_scripts(config: &ConfigYaml, root_fs_path: &str) -> anyhow::Result<()> {
    // Jeśli nie ma żadnych skryptów, po prostu zwracamy Ok
    let scripts = match &config.scripts {
        Some(s) => s,
        None => return Ok(()),
    };

    println!("Executing post-install scripts...");

    for script_path in scripts {
        if !script_path.exists() {
            println!("Skipping missing script: {}", script_path);
            continue;
        }

        // Tworzymy Bubblewrap dla root_fs
        let mut bwrap = build_bwrap_base(root_fs_path)?;

        // Bindujemy katalog ze skryptem w kontenerze
        let script_dir = script_path.parent().unwrap();
        bwrap.bind_read(script_dir.as_str(), script_dir.as_str());

        // Dodajemy skrypt jako polecenie do uruchomienia
        bwrap.append_child_argv([script_path.as_str()]);

        // Uruchamiamy w izolowanym środowisku
        let output = bwrap.run_captured()
            .with_context(|| format!("Failed to execute script {}", script_path))?;

        println!("Script {} output:\n{}", script_path, String::from_utf8_lossy(&output));
    }

    Ok(())
}

fn enable_services(config: &ConfigYaml, root_fs_path: &str) -> anyhow::Result<()> {
    let services = match &config.services {
        Some(s) => s,
        None => return Ok(()),
    };

    println!("Enabling services...");
    for service in services {
        let mut bwrap = build_bwrap_base(root_fs_path)?;
        let command = format!("systemctl enable {}", service);

        bwrap.append_child_argv(["/bin/sh", "-c", &command]);
        let output = bwrap.run_captured()
            .with_context(|| format!("Failed to enable service {}", service))?;
    }

    Ok(())
}

fn generate_initramfs(root_fs: &Dir) -> anyhow::Result<()> {
    let kernel_dirs_opt = find_kernel_dir_fs(root_fs)
        .context("Failed to find kernel directory in root filesystem")?;

    let kernel_dirs = match kernel_dirs_opt {
        Some(ref dir) => vec![dir.clone()],
        None => vec![],
    };

    if kernel_dirs.is_empty() {
        println!("No kernel found, skipping initramfs generation");
    }

    for kernel_dir in kernel_dirs {
        println!("Generating initramfs for kernel in {}", kernel_dir);
        run_dracut(root_fs, kernel_dir.as_str())?;
    }

    Ok(())
}

pub fn compose_post(config: &ConfigYaml, root_fs: &Dir, root_fs_path: &str) -> anyhow::Result<()> {
    // Move from config pacmanConf to root_fs
    if let Some(pacman_conf) = &config.pacmanConf {
        let dest_path = format!("{}/etc/pacman.conf", root_fs_path);
        fs::copy(pacman_conf, &dest_path)
            .with_context(|| format!("Failed to copy pacman.conf from {} to {}", pacman_conf, dest_path))?;
    }

    prepare_rootfs(root_fs)?; // tu możesz dalej używać Dir
    execute_post_scripts(config, root_fs_path)?; // teraz używamy &str
    enable_services(config, root_fs_path)?;
    generate_initramfs(root_fs)?;
    Ok(())
}
