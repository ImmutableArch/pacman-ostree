// Generacja Initramfs przez dracut bez hosta

use std::process::Command;
use anyhow::{Context, Result};
use cap_std::fs::Dir;
use camino::Utf8Path;
use tempfile::tempdir;
use crate::bubblewrap::Bubblewrap;


pub fn run_dracut(root_fs_path: &str, kernel_version: &str) -> Result<()> {
    let mut bwrap = build_bwrap_base(root_fs_path)?;

    let output_path = format!("/lib/modules/{}/initramfs.img", kernel_version);

    bwrap.append_child_argv([
        "dracut",
        "--no-hostonly",
        "--kver",
        kernel_version,
        "--reproducible",
        "-v",
        "--add",
        "ostree",
        "-f",
        &output_path,
    ]);

    bwrap.run()
        .context("Failed to run dracut")?;

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

