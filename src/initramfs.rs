// Generacja Initramfs przez dracut bez hosta

use std::process::Command;
use anyhow::{Context, Result};
use cap_std::fs::Dir;
use camino::Utf8Path;
use tempfile::tempdir;


pub fn run_dracut(root_fs: &Dir, kernel_dir: &str) -> Result<()> {
    let tmp_dir = tempfile::tempdir()?;
    let tmp_initramfs_path = tmp_dir.path().join("initramfs.img");
    Command::new("dracut")
        .args([
            "--no-hostonly",
            "--kver",
            kernel_dir,
            "--reproducible",
            "-v",
            "--add",
            "ostree",
            "-f",
        ])
        .arg(&tmp_initramfs_path)
        .status()?;
    let utf8_tmp_dir_path = Utf8Path::from_path(tmp_dir.path().strip_prefix("/")?)
            .context("Error turning Path to Utf-8 Path")?;
    
    root_fs.rename(utf8_tmp_dir_path.join("initramfs.img"), &root_fs, (Utf8Path::new("lib/modules").join(kernel_dir)).join("initramfs.img"))?;
    Ok(())
}

