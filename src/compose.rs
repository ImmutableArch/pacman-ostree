//Build Arch-Based OSTree OCI Image
//TODO - 2. Preparation logic 3. Rootfs Preparation 4. OSTREE Commit

use std::error::Error;
use camino::Utf8PathBuf;
use tempfile::TempDir;
use tokio::io::AsyncReadExt;
use serde::Deserialize;
use std::borrow::Cow;
use std::num::NonZeroU32;
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use clap::Parser;
use fn_error_context::context;
use oci_spec::image::ImageManifest;
use ostree::gio;
use ostree_ext::containers_image_proxy;
use ostree_ext::glib::prelude::*;
use ostree_ext::keyfileext::{map_keyfile_optional, KeyFileExt};
use ostree_ext::oci_spec::image::ImageConfiguration;
use ostree_ext::ostree::MutableTree;
use ostree_ext::{container as ostree_container, glib};
use ostree_ext::{oci_spec, ostree};
use std::os::unix::fs as unix_fs;

use crate::pacman_manager;

const SYSROOT: &str = "sysroot";
const USR: &str = "usr";
const ETC: &str = "etc";
const OCI_ARCHIVE_TRANSPORT: &str = "oci-archive";

#[derive(Parser, Debug)]
pub struct ComposeImageOpts
{
    /// Max layers to output
    #[clap(long)]
    max_layers: Option<NonZeroU32>,

    /// Config File
    #[clap(value_parser)]
    pub manifest: Utf8PathBuf,

    /// Output musi be with .ociarchive end
    #[clap(value_parser)]
    output: Utf8PathBuf,

    /// OSTree repo
    #[clap(long)]
    ostree_repo: Option<Utf8PathBuf>,
}

#[derive(Debug, Deserialize)]
pub struct ConfigYaml
{
    include: Option<Vec<String>>, //Inne pliki .yaml to tej strukturze
    r#ref: String, //Branch OSTree
    packages: Vec<String>, //Pakiety do instalacji
    repos: Option<Vec<String>>, //Dodatkowe repozytoria
}

impl ConfigYaml
{
    fn merge(&mut self, other: ConfigYaml)
    {
        self.r#ref = other.r#ref;
        self.packages.extend(other.packages);
        // scalanie repos
        match (&mut self.repos, other.repos) {
            (Some(self_repos), Some(other_repos)) => self_repos.extend(other_repos),
            (None, Some(other_repos)) => self.repos = Some(other_repos),
            _ => {} // nic do zrobienia jeśli other.repos == None
        }

        // scalanie include
        match (&mut self.include, other.include) {
            (Some(self_inc), Some(other_inc)) => self_inc.extend(other_inc),
            (None, Some(other_inc)) => self.include = Some(other_inc),
            _ => {} // nic do zrobienia jeśli other.include == None
        }
    }
}

pub fn yaml_parse(path: &str) -> Result<ConfigYaml, Box<dyn Error>> {
    let contents = fs::read_to_string(path)?;
    let mut config: ConfigYaml = serde_yaml::from_str(&contents)?;

    // Wczytaj i scal pliki z `include`
    if let Some(include_files) = config.include.clone() {
        for inc_path in include_files {
            let included = yaml_parse(inc_path.as_str())?;
            config.merge(included);
        }
    }


    Ok(config)
}

fn prepare_rootfs(config: &ConfigYaml) -> std::io::Result<TempDir>
{
    let tmp_dir = TempDir::new()?; // tworzy unikalny katalog w /tmp
    println!("Temporary rootfs directory created at: {:?}", tmp_dir.path());
    let pacman_dir = "var/lib/pacman";
    let path = tmp_dir.path().join(pacman_dir);
    fs::create_dir(path);
    install_filesystem(tmp_dir.path(), &config.packages)?;
    Ok(tmp_dir)
}

fn install_filesystem(rootfs: &Path, packages: &[String]) -> std::io::Result<()>
{
    println!("Installing packages to rootfs");
    pacman_manager::pacstrap_install(&rootfs, packages); //Install packages
    let dirs_to_create = [
        "boot",
        "sysroot",
        "var/home",
        "sysroot/ostree",
    ];
    for dir in dirs_to_create {
        let path = rootfs.join(dir);
        fs::create_dir_all(&path)?;
    }
    let dirs_to_remove = ["var/log", "home", "root", "usr/local", "srv"];
    for dir in dirs_to_remove {
        let path = rootfs.join(dir);
        if path.exists() {
            fs::remove_dir_all(&path)?;
        }
    }
    let symlinks = [
        ("var/home", "home"),
        ("var/roothome", "root"),
        ("var/usrlocal", "usr/local"),
        ("var/srv", "srv"),
        ("sysroot/ostree", "ostree"),
    ];

    for (target, link_name) in &symlinks {
        let target_path = rootfs.join(target);
        let link_path = rootfs.join(link_name);

        // Usuń istniejący link jeśli istnieje
        if link_path.exists() {
            fs::remove_file(&link_path)?;
        }

        unix_fs::symlink(&target_path, &link_path)?;
    }

    Ok(())
}

pub(crate) fn run(config: &ConfigYaml) -> std::io::Result<()>
{
    prepare_rootfs(config);
    Ok(())
}


