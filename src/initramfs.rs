// Generacja Initramfs przez dracut bez hosta

use std::process::Command;
use anyhow::{Context, Result};
use cap_std::fs::Dir;
use camino::Utf8Path;
use tempfile::tempdir;
use std::path::Path;
use crate::bubblewrap::Bubblewrap;


pub fn run_dracut(root_fs_path: &str, kernel_version: &str) -> Result<()> {
    // ścieżka docelowa initramfs
    let output_path = Path::new(root_fs_path)
        .join("lib/modules")
        .join(kernel_version)
        .join("initramfs.img");

    // upewniamy się, że katalog istnieje
    std::fs::create_dir_all(output_path.parent().unwrap())
        .context("Failed to create directories for initramfs")?;

    // uruchamiamy depmod w sysroot
    //let status_depmod = Command::new("depmod")
        //.args(["-b", root_fs_path, kernel_version])
        //.status()
        //.context("Failed to run depmod")?;

    //if !status_depmod.success() {
        //anyhow::bail!("depmod failed");
    //}

    // uruchamiamy dracut z --sysroot
    let status_dracut = Command::new("dracut")
        .args([
            "--no-hostonly",
            "--kver", kernel_version,
            "--reproducible",
            "-v",
            "--add", "ostree",
            "-f",
            output_path.to_str().unwrap(),
            "--sysroot",
            root_fs_path,
        ])
        .status()
        .context("Failed to run dracut")?;

    if !status_dracut.success() {
        anyhow::bail!("dracut failed");
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

