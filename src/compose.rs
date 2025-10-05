//Build Arch-Based OSTree OCI Image
//TODO - Services, Custom scripts, Initramfs generation

use std::error::Error;
use camino::Utf8PathBuf;
use ostree_ext::container::ImageReference;
use tempfile::TempDir;
use tokio::io::AsyncReadExt;
use serde::Deserialize;
use tokio::time::error::Elapsed;
use chrono::{DateTime, FixedOffset, Utc};
use std::borrow::Cow;
use rustix::fs::Dir;
use camino::Utf8Path;
use anyhow::{Result, Context, anyhow};
use std::num::NonZeroU32;
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
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
use crate::container;

const SYSROOT: &str = "sysroot";
const USR: &str = "usr";
const ETC: &str = "etc";
const USR_ETC: &str = "usr/etc";
const OCI_ARCHIVE_TRANSPORT: &str = "oci-archive";

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
    pub ostree_repo: Utf8PathBuf,
}

#[derive(Debug, Deserialize)]
pub struct ConfigYaml
{
    include: Option<Vec<String>>, //Inne pliki .yaml to tej strukturze
    packages: Vec<String>, //Pakiety do instalacji
    services: Option<Vec<String>>, //Usługi systemd do włączenia
    scripts: Option<Vec<Utf8PathBuf>> //Skrypty postinstalacyjne
}

impl ConfigYaml
{
    fn merge(&mut self, other: ConfigYaml)
    {
        self.packages.extend(other.packages);
        // scalanie services
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

fn prepare_rootfs(config: &ConfigYaml) -> Result<TempDir> {
    let tmp_dir = TempDir::new()?; // creates unique dir in /tmp
    println!("Temporary rootfs directory created at: {:?}", tmp_dir.path());

    let pacman_dir = "var/lib/pacman";
    let path = tmp_dir.path().join(pacman_dir);
    fs::create_dir_all(&path).with_context(|| format!("creating pacman dir at {:?}", path))?;

    // Install files, propagate errors
    install_filesystem(tmp_dir.path(), &config.packages)
        .context("Failed to install filesystem (pacstrap)")?;

    setup_rootfs_services(tmp_dir.path(), config.services.as_deref())
        .context("Failed to enable services")?;

    execute_scripts_in_rootfs(tmp_dir.path(), config.scripts.as_deref())
        .context("Failed to execute scripts")?;

    rebuild_initramfs(tmp_dir.path())
        .context("Failed to build initramfs")?;

    Ok(tmp_dir)
}

fn install_filesystem(rootfs: &Path, packages: &[String]) -> Result<()> {
    println!("Installing packages to rootfs at {:?}", rootfs);

    // call pacstrap_install from pacman_manager (now returns Result)
    crate::pacman_manager::pacstrap_install(rootfs, packages)
        .context("pacstrap_install failed")?;

    // create required dirs (if not existing)
    let dirs_to_create = [
        "boot",
        "sysroot",
        "var/home",
        "sysroot/ostree",
        "var/roothome"
    ];
    for dir in dirs_to_create {
        let path = rootfs.join(dir);
        fs::create_dir_all(&path)
            .with_context(|| format!("creating dir {:?}", path))?;
    }

    // remove unwanted dirs
    let dirs_to_remove = ["var/log", "home", "root", "usr/local", "srv"];
    for dir in dirs_to_remove {
        let path = rootfs.join(dir);
        if path.exists() {
            fs::remove_dir_all(&path)
                .with_context(|| format!("removing dir {:?}", path))?;
        }
    }

    // create symlinks
    let symlinks = [
        ("var/home", "home"),
        ("var/roothome", "root"),
        ("var/usrlocal", "usr/local"),
        ("var/srv", "srv"),
        ("sysroot/ostree", "ostree"),
    ];

    for (target, link_name) in &symlinks {
        let link_path = rootfs.join(link_name);
        let target_path = Path::new(target); // relatywnie do rootfs

        if link_path.exists() {
            fs::remove_file(&link_path)?;
        }
        unix_fs::symlink(target_path, &link_path)?;
    }


    // attempt to strip usermeta (propagate error)
    let mut info = XattrRemovalInfo::default();
    strip_usermeta(&rootfs, &mut info)
        .with_context(|| format!("strip_usermeta on {:?}", rootfs))?;
    if info.count > 0 {
        eprintln!("Found unhandled xattrs in files: {}", info.count);
        for attr in info.names {
            eprintln!("  {attr:?}");
        }
    }

    // Debug: show top-level contents to help debugging when user said "nothing written"
    println!("Rootfs top-level after pacstrap:");
    match std::fs::read_dir(rootfs) {
        Ok(rd) => {
            for e in rd.flatten().take(50) {
                println!(" - {:?}", e.file_name());
            }
        }
        Err(e) => {
            eprintln!("Failed to list rootfs after install: {e}");
        }
    }

    Ok(())
}

fn setup_rootfs_services(rootfs: &Path, services: Option<&[String]>) -> Result<()>
{
    if let Some(services) = services
    {
        for service in services
        {
            println!("Enabling service: {}", service);

            let status = Command::new("arch-chroot")
                .arg(rootfs)
                .arg("systemctl")
                .arg("enable")
                .arg(service)
                .status()
                .with_context(|| format!("Failed to run systemctl enable {service} in chroot"))?;

            if !status.success()
            {
                eprintln!("Failed to enable service: {}", service);
            }

        }
    }

    Ok(())
}

fn execute_scripts_in_rootfs(rootfs: &Path, scripts: Option<&[Utf8PathBuf]>) -> Result<()>
{
    if let Some(scripts) = scripts
    {
        for script in scripts
        {
            println!("Executing script: {}", script);

            let file = File::open(script)
                .with_context(|| format!("Failed to open script file {}", script))?;

            let status = Command::new("arch-chroot")
                .arg(rootfs)
                .arg("/bin/bash")
                .arg("-s")
                .stdin(file)
                .status()
                .with_context(|| format!("Failed to execute {script} in chroot"))?;

            if !status.success()
            {
                eprintln!("Failed to execute script: {}", script);
            }

        }
    }
    Ok(())
}

///Regenerate Initramfs with ostree
fn rebuild_initramfs(rootfs: &Path) -> Result<()> {
    // 1. Znajdź katalog kernela w rootfs/usr/lib/modules
    let output = Command::new("bash")
    .arg("-c")
    .arg(format!(
        "basename $(find \"{}/usr/lib/modules\" -maxdepth 1 -type d | grep -v -E \"\\.img\" | tail -n 1)",
                 rootfs.display()
    ))
    .output()
    .context("Failed to detect kernel version in rootfs")?;

    if !output.status.success() {
        eprintln!(
            "Failed to detect kernel version: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let kernel_version = String::from_utf8(output.stdout)?
    .trim()
    .to_string();

    println!("Kernel version detected: {}", kernel_version);

    // 2. Ścieżka docelowa initramfs wewnątrz rootfs
    let initramfs_path = format!("{}/usr/lib/modules/{}/initramfs.img", rootfs.display(), kernel_version);

    // 3. Uruchom dracut na hoście, wskazując ścieżki do rootfs
    let status = Command::new("dracut")
    .arg("--force")
    .arg("-r")
    .arg(&rootfs)
    .arg("--no-hostonly")
    .arg("--zstd")
    .arg("--reproducible")
    .arg("--verbose")
    .arg("--kver")
    .arg(&kernel_version)
    .arg("--add")
    .arg("ostree")
    .arg(&initramfs_path)
    .status()
    .context("Failed to run dracut on host")?;

    if !status.success() {
        eprintln!("dracut failed on host");
    }

    Ok(())
}

fn fdpath_for(fd: impl AsFd, path: impl AsRef<Path>) -> PathBuf {
    let fd = fd.as_fd();
    let path = path.as_ref();
    let mut fdpath = PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()));
    fdpath.push(path);
    fdpath
}

/// Get an optional extended attribute from the path; does not follow symlinks on the end target.
fn lgetxattr_optional_at(
    fd: impl AsFd,
    path: impl AsRef<Path>,
    key: impl AsRef<OsStr>,
) -> std::io::Result<Option<Vec<u8>>> {
    let fd = fd.as_fd();
    let path = path.as_ref();
    let key = key.as_ref();

    // Arbitrary hardcoded value, but we should have a better xattr API somewhere
    let mut value = [0u8; 8196];
    let fdpath = fdpath_for(fd, path);
    match rustix::fs::lgetxattr(&fdpath, key, &mut value) {
        Ok(r) => Ok(Some(Vec::from(&value[0..r]))),
        Err(e) if e == rustix::io::Errno::NODATA => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[derive(Debug, Default)]
struct XattrRemovalInfo {
    /// Set of unhandled xattrs we found
    names: BTreeSet<OsString>,
    /// Number of files with unhandled xattrsi
    count: u64,
}

fn strip_usermeta(dir_path: &Path, info: &mut XattrRemovalInfo) -> Result<()> {
    let usermeta_key = "user.ostreemeta";

    // Open dir as File, so we can pass an FD to fdpath_for / lgetxattr_optional_at
    let dir_file = File::open(dir_path)
        .with_context(|| format!("opening directory {:?}", dir_path))?;

    for entry in fs::read_dir(dir_path)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let name = entry.file_name();

        if ty.is_dir() {
            strip_usermeta(&entry.path(), info)?;
        } else {
            let Some(usermeta) = lgetxattr_optional_at(&dir_file, &name, usermeta_key)? else {
                continue;
            };
            let usermeta =
                glib::Variant::from_data::<(u32, u32, u32, Vec<(Vec<u8>, Vec<u8>)>), _>(usermeta);
            let xattrs = usermeta.child_value(3);
            let n = xattrs.n_children();
            for i in 0..n {
                let v = xattrs.child_value(i);
                let key = v.child_value(0);
                let key = key.fixed_array::<u8>().unwrap();
                let key = OsStr::from_bytes(key);
                if !info.names.contains(key) {
                    info.names.insert(key.to_owned());
                }
                info.count += 1;
            }
            let fdpath = fdpath_for(&dir_file, &name);
            rustix::fs::lremovexattr(&fdpath, usermeta_key).context("lremovexattr")?;
        }
    }

    Ok(())
}

fn label_to_xattrs(label: Option<&str>) -> Option<glib::Variant> {
    let xattrs = label.map(|label| {
        let mut label: Vec<_> = label.to_owned().into();
        label.push(0);
        vec![(c"security.selinux".to_bytes_with_nul(), label)]
    });
    xattrs.map(|x| x.to_variant())
}

fn create_root_dirmeta(root_path: &Path, policy: &ostree::SePolicy) -> Result<glib::Variant> {
    let finfo = gio::FileInfo::new();
    let meta = std::fs::metadata(root_path)?;  // tu masz UID, GID, tryb
    finfo.set_attribute_uint32("unix::uid", meta.uid());
    finfo.set_attribute_uint32("unix::gid", meta.gid());
    finfo.set_attribute_uint32("unix::mode", libc::S_IFDIR | meta.mode() as u32);

    let label = policy.label("/", 0o777 | libc::S_IFDIR, gio::Cancellable::NONE)?;
    let xattrs = label_to_xattrs(label.as_deref());
    let r = ostree::create_directory_metadata(&finfo, xattrs.as_ref());
    Ok(r)
}


enum MtreeEntry {
    #[allow(dead_code)]
    Leaf(String),
    Directory(MutableTree),
}

impl MtreeEntry {
    fn require_dir(self) -> Result<MutableTree> {
        match self {
            MtreeEntry::Leaf(_) => anyhow::bail!("Expected a directory"),
            MtreeEntry::Directory(t) => Ok(t),
        }
    }
}

/// The two returns value in C are mutually exclusive; also map "not found" to None.
fn mtree_lookup(t: &ostree::MutableTree, path: &str) -> Result<Option<MtreeEntry>> {
    let r = match t.lookup(path) {
        Ok((Some(leaf), None)) => Some(MtreeEntry::Leaf(leaf.into())),
        Ok((_, Some(subdir))) => Some(MtreeEntry::Directory(subdir)),
        Ok((None, None)) => unreachable!(),
        Err(e) if e.matches(gio::IOErrorEnum::NotFound) => None,
        Err(e) => return Err(e.into()),
    };
    Ok(r)
}

fn postprocess_mtree(repo: &ostree::Repo, rootfs: &ostree::MutableTree) -> Result<()> {
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


fn clean_rootfs(root: &Utf8Path) -> anyhow::Result<()> {
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry?;
        let ftype = entry.file_type();

        if ftype.is_socket() || ftype.is_fifo() || ftype.is_char_device() || ftype.is_block_device() {
            let path = entry.path();
            println!("⚠️ Removing special file: {}", path.display());
            // Możesz usunąć fizycznie:
            std::fs::remove_file(path)?;
            // albo tylko zostawić jako warning i nic nie robić
        }
    }
    Ok(())
}

fn generate_commit_from_rootfs(
    repo: &ostree::Repo,
    rootfs: &Utf8Path,  // zamiast &Dir
    modifier: ostree::RepoCommitModifier,
    creation_time: Option<&chrono::DateTime<chrono::FixedOffset>>,
) -> Result<String> {
    //Fix sockets issue
    clean_rootfs(rootfs)?;
    let root_mtree = ostree::MutableTree::new();
    let cancellable = gio::Cancellable::NONE;
    let tx = repo.auto_transaction(cancellable)?;

    let rootfs_dir = File::open(rootfs.as_std_path())
    .context("Opening rootfs dir for SePolicy")?;
    let policy = ostree::SePolicy::new_at(rootfs_dir.as_raw_fd(), cancellable)?;
    modifier.set_sepolicy(Some(&policy));

    let root_dirmeta = create_root_dirmeta(rootfs.as_std_path(), &policy)?;
    let root_metachecksum = repo
        .write_metadata(
            ostree::ObjectType::DirMeta,
            None,
            &root_dirmeta,
            cancellable,
        )
        .context("Writing root dirmeta")?;
    root_mtree.set_metadata_checksum(&root_metachecksum.to_hex());

    for entry in std::fs::read_dir(rootfs)? {
        let entry = entry?;
        let name = entry.file_name().into_string().unwrap(); // jeśli UTF-8

        let ftype = entry.file_type()?;
        if ftype.is_dir() && name == SYSROOT {
            let child_mtree = root_mtree.ensure_dir(&name)?;
            child_mtree.set_metadata_checksum(&root_metachecksum.to_hex());
        } else if ftype.is_dir() {
            let child_mtree = root_mtree.ensure_dir(&name)?;
            let child_path = entry.path();
            let dir_file = std::fs::File::open(&child_path)?;
            repo.write_dfd_to_mtree(
            dir_file.as_raw_fd(),
           ".",
                &child_mtree,
      Some(&modifier),
                cancellable,
            )?;
        } else if ftype.is_symlink() {
            let contents = std::fs::read_link(entry.path())?;
            let selabel_path = format!("/{name}");
            let label = policy.label(selabel_path.as_str(), 0o777 | libc::S_IFLNK, cancellable)?;
            let xattrs = label_to_xattrs(label.as_deref());
            let link_checksum = repo
                .write_symlink(None, 0, 0, xattrs.as_ref(), contents.to_str().unwrap(), cancellable)
                .with_context(|| format!("Processing symlink {selabel_path}"))?;
            root_mtree.replace_file(&name, &link_checksum)?;
        } else if ftype.is_socket() {
            println!("⚠️ Ignoring socket {name}");
            continue;
        } else if ftype.is_fifo() {
            println!("⚠️ Ignoring FIFO {name}");
            continue;
        } else if ftype.is_char_device() || ftype.is_block_device() {
            println!("⚠️ Ignoring device node {name}");
            continue;
        } else {
            anyhow::bail!("Unsupported special file {name} at toplevel");
        }
    }

    postprocess_mtree(repo, &root_mtree)?;

    let ostree_root = repo.write_mtree(&root_mtree, cancellable)?;
    let ostree_root = ostree_root
        .downcast_ref::<ostree::RepoFile>()
        .ok_or_else(|| anyhow::anyhow!("Failed to cast to RepoFile"))?;
    let creation_time: u64 = creation_time
        .as_ref()
        .map(|t| t.timestamp())
        .unwrap_or_default()
        .try_into()
        .context("Parsing creation time")?;
    let commit = match repo.write_commit_with_time(
        None,
        None,
        None,
        None,
        ostree_root,
        creation_time,
        cancellable,
    ) {
        Ok(c) => {
            tx.commit(cancellable)
                .context("Committing transaction")?;
            c
        }
        Err(e) => {
            return Err(anyhow::anyhow!("Commit failed: {e}"));
        }
    };
    Ok(commit.into())
}


pub(crate) async fn run(config: &ConfigYaml, opts: &ComposeImageOpts) {
    if let Err(e) = run_inner(config, opts).await {
        eprintln!("ERROR: {:?}", e);
        std::process::exit(1);
    }
}

async fn run_inner(config: &ConfigYaml, opts: &ComposeImageOpts) -> Result<()> {
    if !opts.ostree_repo.exists() {
        return Err(anyhow!("OSTree repo path does not exist: {}", opts.ostree_repo));
    }
    if !opts.ostree_repo.is_dir() {
        return Err(anyhow!("OSTree repo path is not a directory: {}", opts.ostree_repo));
    }

    println!("Using OSTree repo at {}", opts.ostree_repo);

    let _rootfs = prepare_rootfs(config)?;
    let rootfs_path: &Utf8Path = Utf8Path::from_path(_rootfs.path())
        .context("Rootfs path is not valid UTF-8")?;

    let modifier = ostree::RepoCommitModifier::new(
        ostree::RepoCommitModifierFlags::SKIP_XATTRS
            | ostree::RepoCommitModifierFlags::CANONICAL_PERMISSIONS,
        None,
    );

    let now_utc: DateTime<Utc> = Utc::now();
    let creation_time: DateTime<FixedOffset> = now_utc.into();

    let repo = ostree::Repo::open_at(libc::AT_FDCWD, &opts.ostree_repo.as_str(), gio::Cancellable::NONE)?;
    println!("Generating Commit...");
    let commit = generate_commit_from_rootfs(&repo, rootfs_path, modifier, Some(&creation_time))?;
    //Generate container image

    println!("✅ Commit {commit} exported to {}", opts.output);
    println!("🔹 Building OCI image from commit...");

    let imgref_str = format!("oci-archive:{}", opts.output);
    let args = vec![
    "container-encapsulate".to_string(),
    "--repo".to_string(),
    opts.ostree_repo.to_string(),
    commit.clone(),
    imgref_str,
    "--format-version".to_string(),
    "2".to_string(),
    "--max-layers".to_string(),
    opts.max_layers.map(|v| v.to_string()).unwrap_or("64".to_string()), // np. domyślnie 64
    ];


    container::container_encapsulate(args)
        .context("Failed to encapsulate container image")?;
    Ok(())
}



