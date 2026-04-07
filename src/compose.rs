// Build Arch Linux OSTree image 

use cap_std::{ambient_authority, fs::Dir};
use nix::sys::prctl::get_child_subreaper;
//Compose config yaml structure
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, os::fd::AsRawFd};
use serde_yaml;
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};
use clap::Parser;
use ostree_ext::{glib::translate::Stash, ostree::{self, RepoCommitModifier, RepoCommitModifierFlags, RepoFile, RepoMode, SePolicy}};
use ostree_ext::{gio, glib};
use glib::prelude::*;
use ostree::MutableTree;
use ostree::Repo;
use crate::package_installer::{self, install_packages};
use std::num::NonZeroU32;
use std::error::Error;
use camino::Utf8PathBuf;
use tempfile::TempDir;
use cap_std::AmbientAuthority;
use std::os::unix::fs as unix_fs;
use anyhow::Context;
use anyhow::{anyhow, Result};
use crate::composepost;

#[derive(Parser, Debug)]
pub struct ComposeImageOpts
{
    /// Max layers to output
    #[clap(long)]
    pub max_layers: Option<NonZeroU32>,

    /// Config File
    #[clap(value_parser)]
    pub manifest: Utf8PathBuf,

    /// Output musi be with .ociarchive end
    #[clap(value_parser)]
    pub output: Utf8PathBuf,

    /// OSTree repo
    #[clap(long)]
    pub ostree_repo: Option<Utf8PathBuf>,
}

#[derive(Debug, Deserialize)]
pub struct ConfigYaml
{
    pub include: Option<Vec<String>>, //Inne pliki .yaml to tej strukturze
    pub r#ref: String, //Branch OSTree
    pub packages: Vec<String>, //Pakiety do instalacji
    pub services: Option<Vec<String>>,
    pub scripts: Option<Vec<Utf8PathBuf>>,
    pub pacmanConf: Option<String>, //Niestandardowy plik pacman.conf
}

impl ConfigYaml
{
    fn merge(&mut self, other: ConfigYaml)
    {
        self.r#ref = other.r#ref;
        self.packages.extend(other.packages);
        self.pacmanConf = other.pacmanConf.or(self.pacmanConf.clone());

        // scalanie include
        match (&mut self.include, other.include) {
            (Some(self_inc), Some(other_inc)) => self_inc.extend(other_inc),
            (None, Some(other_inc)) => self.include = Some(other_inc),
            _ => {} // nic do zrobienia jeśli other.include == None
        }

        match (&mut self.services, other.services) {
            (Some(self_services), Some(other_services)) => self_services.extend(other_services),
            (None, Some(other_services)) => self.services = Some(other_services),
            _ => {} // nic do zrobienia jeśli other.repos == None
        }

        //scalanie scripts
        match (&mut self.scripts, other.scripts) {
            (Some(self_scripts), Some(other_scripts)) => self_scripts.extend(other_scripts),
            (None, Some(other_scripts)) => self.scripts = Some(other_scripts),
            _ => {} // nic do zrobienia jeśli other.repos == None
        }
    }
}

pub fn yaml_parse(path: &str) -> anyhow::Result<ConfigYaml> {
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

const SYSROOT_PREFIX: &str = "/sysroot/";
const USR: &str = "usr";
const ETC: &str = "etc";
const USR_ETC: &str = "usr/etc";
const OCI_ARCHIVE_TRANSPORT: &str = "oci-archive";

pub async fn compose_image(opts: ComposeImageOpts) -> anyhow::Result<()> {
    println!("Reading config from: {}", opts.manifest);
    //Sprawdzenie czy plik istnieje
    if !opts.manifest.exists() {
        return Err(anyhow!("Config file {} does not exist", opts.manifest));
    }
    let config = yaml_parse(opts.manifest.as_str())?;
    //Stworzenie tymczasowego katalogu do pracy
    let temp_dir = TempDir::new()?;
    let temp_dir_cap = Dir::open_ambient_dir(temp_dir.path(), ambient_authority())?;
    println!("Using temporary directory: {}", temp_dir.path().display());
    let pacman_conf = config.pacmanConf.as_ref().map(|s| vec![s.clone()]);


    install_packages_compose(&temp_dir, config.packages.clone(), pacman_conf).await?;
    composepost::compose_post(
        &config,               // &ConfigYaml
        &temp_dir_cap,         // &Dir
     temp_dir.path().to_str().unwrap(), // &str
    )?;

    let repo_path = opts.ostree_repo.as_ref().map(|p| p.as_str()).unwrap();
    if !Path::new(repo_path).exists() {
        println!("Creating new OSTree repo at {}", repo_path);
        Repo::create_at(libc::AT_FDCWD, repo_path, RepoMode::BareUser, None, gio::Cancellable::NONE)?;
    }
    let repo = Repo::open_at(libc::AT_FDCWD, repo_path, gio::Cancellable::NONE)?;
    let creation_time = chrono::Utc::now().with_timezone(&chrono::FixedOffset::east(0));
    generate_commit_from_rootfs(&repo, &temp_dir_cap, Some(&creation_time))?;
    Ok(())
}

///Install package to OSTree tree
pub async fn install_packages_compose(
    dir: &TempDir,
    package_names: Vec<String>,
    pacman_conf: Option<Vec<String>>,
) -> anyhow::Result<()> {

    // Vec<String> → Vec<&str>
    let pkg_refs: Vec<&str> = package_names
        .iter()
        .map(|s| s.as_str())
        .collect();

    // TempDir → &str
    let root = dir.path().to_str().ok_or(anyhow!("Invalid path"))?;

    // Option<Vec<String>> → Option<&str>
    let pacman_conf_ref: Option<&str> = pacman_conf
        .as_ref()
        .and_then(|v| v.first())
        .map(|s| s.as_str());
    install_packages(pkg_refs, root, pacman_conf_ref).await?;
    Ok(())
}

enum MtreeEntry {
    #[allow(dead_code)]
    Leaf(String),
    Directory(MutableTree),
}

impl MtreeEntry {
    fn require_dir(self) -> anyhow::Result<MutableTree> {
        match self {
            MtreeEntry::Leaf(_) => anyhow::bail!("Expected a directory"),
            MtreeEntry::Directory(t) => Ok(t),
        }
    }
}

fn mtree_lookup(t: &ostree::MutableTree, path: &str) -> anyhow::Result<Option<MtreeEntry>> {
    let r = match t.lookup(path) {
        Ok((Some(leaf), None)) => Some(MtreeEntry::Leaf(leaf.into())),
        Ok((_, Some(subdir))) => Some(MtreeEntry::Directory(subdir)),
        Ok((None, None)) => unreachable!(),
        Err(e) if e.matches(gio::IOErrorEnum::NotFound) => None,
        Err(e) => return Err(e.into()),
    };
    Ok(r)
}

fn postprocess_mtree(repo: &ostree::Repo, rootfs: &ostree::MutableTree) -> anyhow::Result<()> {
    let etc_subdir = mtree_lookup(rootfs, ETC)?
        .map(|e| e.require_dir().context("/etc"))
        .transpose()?;
    let usr_etc_subdir = mtree_lookup(rootfs, USR_ETC)?
        .map(|e| e.require_dir().context("/usr/etc"))
        .transpose()?;
    match (etc_subdir, usr_etc_subdir) {
        (None, None) => {
            // No /etc? We'll let you try it.
        }
        (None, Some(_)) => {
            // Having just /usr/etc is the expected ostree default.
        }
        (Some(etc), None) => {
            // We need to write the etc dir now to generate checksums,
            // then move it.
            repo.write_mtree(&etc, gio::Cancellable::NONE)?;
            let usr = rootfs
                .lookup(USR)?
                .1
                .ok_or_else(|| anyhow!("Missing /usr"))?;
            let usretc = usr.ensure_dir(ETC)?;
            usretc.set_contents_checksum(&etc.contents_checksum());
            usretc.set_metadata_checksum(&etc.metadata_checksum());
            rootfs.remove(ETC, false)?;
        }
        (Some(_), Some(_)) => {
            anyhow::bail!("Found both /etc and /usr/etc");
        }
    }
    Ok(())
}

fn generate_commit_from_rootfs(repo: &Repo, rootfs: &Dir, creation_time: Option<&chrono::DateTime<chrono::FixedOffset>>) -> anyhow::Result<String> {
    let root_mtree = MutableTree::new();
    let cancellable = gio::Cancellable::NONE;
    let tx = repo.auto_transaction(cancellable)?;
    let modifier = RepoCommitModifier::new(
        RepoCommitModifierFlags::SKIP_XATTRS |
        RepoCommitModifierFlags::CANONICAL_PERMISSIONS,
        None
    );

    repo.write_dfd_to_mtree(
        rootfs.as_raw_fd(),
        ".",
        &root_mtree,
        Some(&modifier),
        cancellable,
    )?;

    postprocess_mtree(repo, &root_mtree)?;

    let ostree_root = repo.write_mtree(&root_mtree, cancellable)?;
    let ostree_root = ostree_root.downcast_ref::<RepoFile>().unwrap();
    let creation_time: u64 = creation_time
        .as_ref()
        .map(|t| t.timestamp())
        .unwrap_or_default()
        .try_into()
        .context("Parsing creation time")?;

    let mut commitmeta = glib::VariantDict::new(None);

    let commit = repo.write_commit_with_time(
        None, 
        None, 
        None, 
        Some(&commitmeta.end()), 
        ostree_root, 
        creation_time, 
        cancellable
    )?;

    tx.commit(cancellable)?;
    Ok(commit.into())
}
