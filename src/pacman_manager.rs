// Pacman Helper functions
use std::process::Command;

pub fn install(root: &str, cache: &str, packages: &str)
{
    println!("Downloading and Installing packages...");
    let pacman = Command::new("pacman")
        .arg("-Sy")                           // update databases
        .arg("-r").arg(root)                  // set root
        .arg(format!("--cachedir={}", cache)) // set cache dir
        .arg("--noconfirm")                   // skip prompts
        .arg(packages)
        .status()
        .expect("Failed to install packages");
}

pub fn remove(root: &str, cache: &str, packages: &str)
{
    println!("Removing packages...");
    let pacman = Command::new("pacman")
    .arg("-Rns")                          // remove including deps
    .arg("-r").arg(root)
    .arg(format!("--cachedir={}", cache))
    .arg("--noconfirm")
    .arg(packages)
    .status()
    .expect("Failed to remove packages");
}
