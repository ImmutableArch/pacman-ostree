// Build Arch Linux OSTree image 

use cap_std::{ambient_authority, fs::Dir};
//Compose config yaml structure
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use serde_yaml;
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};
use clap::Parser;
use ostree_ext::ostree;
use ostree::MutableTree;
use ostree::Repo;
use crate::package_installer::{self, install_packages};
use std::num::NonZeroU32;
use std::error::Error;
use camino::Utf8PathBuf;
use tempfile::TempDir;
use cap_std::AmbientAuthority;
use std::os::unix::fs as unix_fs;
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

pub async fn compose_image(opts: ComposeImageOpts) -> Result<(), Box<dyn Error>> {
    println!("Reading config from: {}", opts.manifest);
    //Sprawdzenie czy plik istnieje
    if !opts.manifest.exists() {
        return Err(format!("Config file {} does not exist", opts.manifest).into());
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
    Ok(())
}

///Install package to OSTree tree
pub async fn install_packages_compose(
    dir: &TempDir,
    package_names: Vec<String>,
    pacman_conf: Option<Vec<String>>,
) -> Result<(), Box<dyn Error>> {

    // Vec<String> → Vec<&str>
    let pkg_refs: Vec<&str> = package_names
        .iter()
        .map(|s| s.as_str())
        .collect();

    // TempDir → &str
    let root = dir.path().to_str().ok_or("Invalid path")?;

    // Option<Vec<String>> → Option<&str>
    let pacman_conf_ref: Option<&str> = pacman_conf
        .as_ref()
        .and_then(|v| v.first())
        .map(|s| s.as_str());
    install_packages(pkg_refs, root, pacman_conf_ref).await?;
    Ok(())
}

