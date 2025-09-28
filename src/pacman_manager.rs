// Pacman Helper functions
use std::process::Command;
use std::path::{Path, PathBuf};

pub(crate) fn install(root: &Path, cache: &str, packages: &[String])
{
    println!("Downloading and Installing packages...");
    let pacman = Command::new("pacman")
        .arg("-Sy")                           // update databases
        .arg("-r").arg(root)                  // set root
        .arg(format!("--cachedir={}", cache)) // set cache dir
        .arg("--noconfirm")                   // skip prompts
        .args(packages)
        .status()
        .expect("Failed to install packages");
}

pub(crate) fn remove(root: &Path, cache: &str, packages: &[String])
{
    println!("Removing packages...");
    let pacman = Command::new("pacman")
    .arg("-Rns")                          // remove including deps
    .arg("-r").arg(root)
    .arg(format!("--cachedir={}", cache))
    .arg("--noconfirm")
    .args(packages)
    .status()
    .expect("Failed to remove packages");
}
