///ostree-ext integration
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::CStr;
use std::fmt::Debug;
use std::fs::File;
use std::io::BufReader;
use std::num::NonZeroU32;
use std::path::Path;

use std::process::Command;
use std::rc::Rc;

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use cap_std::fs::Dir;
use cap_std_ext::cap_std;
use cap_std_ext::prelude::*;
use chrono::prelude::*;
use clap::Parser;
use fn_error_context::context;
use ostree::glib;
use ostree_ext::chunking::ObjectMetaSized;
use ostree_ext::container::{Config, ExportOpts, ImageReference};
use ostree_ext::containers_image_proxy;
use ostree_ext::objectsource::{
    ContentID, ObjectMeta, ObjectMetaMap, ObjectMetaSet, ObjectSourceMeta,
};
use ostree_ext::oci_spec::image::{Arch, Os, PlatformBuilder};
use ostree_ext::prelude::*;
use ostree_ext::{gio, oci_spec, ostree};
use crate::pacman_manager;

const COMPONENT_XATTR: &CStr = c"user.component";

#[derive(Debug, Parser)]
pub struct ContainerEncapsulateOpts {
    #[clap(long)]
    #[clap(value_parser)]
    pub repo: Utf8PathBuf,

    /// OSTree branch name or checksum
    pub ostree_ref: String,

    /// Image reference, e.g. registry:quay.io/exampleos/exampleos:latest
    #[clap(value_parser = ostree_ext::cli::parse_base_imgref)]
    pub imgref: ImageReference,

    /// Additional labels for the container
    #[clap(name = "label", long, short)]
    pub labels: Vec<String>,

    /// Path to container image configuration in JSON format.  This is the `config`
    /// field of https://github.com/opencontainers/image-spec/blob/main/config.md
    #[clap(long)]
    pub image_config: Option<Utf8PathBuf>,

    /// Override the architecture.
    #[clap(long)]
    pub arch: Option<Arch>,

    /// Propagate an OSTree commit metadata key to container label
    #[clap(name = "copymeta", long)]
    pub copy_meta_keys: Vec<String>,

    /// Propagate an optionally-present OSTree commit metadata key to container label
    #[clap(name = "copymeta-opt", long)]
    pub copy_meta_opt_keys: Vec<String>,

    /// Corresponds to the Dockerfile `CMD` instruction.
    #[clap(long)]
    pub cmd: Option<Vec<String>>,

    /// Maximum number of container image layers
    #[clap(long)]
    pub max_layers: Option<NonZeroU32>,

    /// The encapsulated container format version; must be 1 or 2.
    #[clap(long, default_value = "1")]
    pub format_version: u32,

    #[clap(long)]
    /// Output content metadata as JSON
    pub write_contentmeta_json: Option<Utf8PathBuf>,

    /// Compare OCI layers of current build with another(imgref)
    #[clap(name = "compare-with-build", long)]
    pub compare_with_build: Option<String>,

    /// Prevent a change in packing structure by taking a previous build metadata (oci config and
    /// manifest)
    #[clap(long)]
    pub previous_build_manifest: Option<Utf8PathBuf>,
}

#[derive(Debug)]
struct MappingBuilder {
    /// Maps from package ID to metadata
    packagemeta: ObjectMetaSet,

    /// Maps from component ID to metadata
    componentmeta: ObjectMetaSet,

    /// Component IDs encountered during filesystem walk for efficient lookup
    component_ids: HashSet<String>,

    /// Maps from object checksum to absolute filesystem path
    checksum_paths: BTreeMap<String, BTreeSet<Utf8PathBuf>>,

    /// Maps from absolute filesystem path to the package IDs that
    /// provide it
    path_packages: HashMap<Utf8PathBuf, BTreeSet<ContentID>>,

    /// Maps from absolute filesystem path to component IDs (for exclusive layers)
    path_components: HashMap<Utf8PathBuf, BTreeSet<ContentID>>,

    unpackaged_id: ContentID,

    /// Files that were processed before the global tree walk
    skip: HashSet<Utf8PathBuf>,

    /// Size according to Pacman database
    pkgsize: u64,
}

impl MappingBuilder {
    /// For now, we stick everything that isn't a package inside a single "unpackaged" state.
    /// In the future though if we support e.g. containers in /usr/share/containers or the
    /// like, this will need to change.
    const UNPACKAGED_ID: &'static str = "pacmanostree-unpackaged-content";

    fn duplicate_objects(&self) -> impl Iterator<Item = (&String, &BTreeSet<Utf8PathBuf>)> {
        self.checksum_paths
            .iter()
            .filter(|(_, paths)| paths.len() > 1)
    }

    fn multiple_owners(&self) -> impl Iterator<Item = (&Utf8PathBuf, &BTreeSet<ContentID>)> {
        self.path_packages.iter().filter(|(_, pkgs)| pkgs.len() > 1)
    }
}

// loop over checksum_paths (this is the entire list of files)
// check if there is a mapping for the file in the "explicit" mapping
// check if there is a mapping for the file in the package mapping
// otherwise put it in the unpackaged bucket
impl MappingBuilder {
    fn create_meta(&self) -> (ObjectMeta, BTreeMap<ContentID, Vec<(Utf8PathBuf, String)>>) {
        let mut package_content = ObjectMetaMap::default();
        // Build map with content_id -> Vec<(checksum, path)> for components
        let mut component_content_map = BTreeMap::new();

        for (checksum, paths) in &self.checksum_paths {
            for path in paths {
                if let Some(component_ids) = self.path_components.get(path) {
                    if let Some(content_id) = component_ids.first() {
                        component_content_map
                            .entry(content_id.clone())
                            .or_insert_with(Vec::new)
                            .push((path.clone(), checksum.clone()));
                    }
                } else if let Some(package_ids) = self.path_packages.get(path) {
                    if let Some(content_id) = package_ids.first() {
                        package_content.insert(checksum.clone(), content_id.clone());
                    }
                } else {
                    package_content.insert(checksum.clone(), self.unpackaged_id.clone());
                }
            }
        }

        let package_meta = ObjectMeta {
            map: package_content,
            set: self.packagemeta.clone(),
        };

        (package_meta, component_content_map)
    }
}

fn build_fs_mapping_recurse(
    path: &mut Utf8PathBuf,
    dir: &gio::File,
    state: &mut MappingBuilder,
    parent_component: Option<String>,
) -> Result<()> {
    let e = dir.enumerate_children(
        "standard::name,standard::type",
        gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
        gio::Cancellable::NONE,
    )?;
    for child in e {
        let childi = child?;
        let name: Utf8PathBuf = childi.name().try_into()?;
        let child = dir.child(&name);
        path.push(&name);
        match childi.file_type() {
            gio::FileType::Regular | gio::FileType::SymbolicLink => {
                let child = child.downcast::<ostree::RepoFile>().unwrap();

                // Remove the skipped path, since we can't hit it again.
                if state.skip.remove(Utf8Path::new(path)) {
                    path.pop();
                    continue;
                }

                // Try to read user.component xattr to identify component-based chunks
                let file_component = get_user_component_xattr(&child)?;
                let effective_component = file_component.or_else(|| parent_component.clone());

                if let Some(component_name) = effective_component {
                    let component_id = Rc::from(component_name.clone());

                    // Track component ID for later processing
                    state.component_ids.insert(component_name);

                    // Associate this path with the component
                    state
                        .path_components
                        .entry(path.clone())
                        .or_default()
                        .insert(Rc::clone(&component_id));
                };

                // Ensure there's a checksum -> path entry. If it was previously
                // accounted for by a package or component, this is essentially a no-op. If not,
                // there'll be no corresponding path -> package entry, and the packaging
                // operation will treat the file as being "unpackaged".
                let checksum = child.checksum().to_string();

                state
                    .checksum_paths
                    .entry(checksum)
                    .or_default()
                    .insert(path.clone());
            }
            gio::FileType::Directory => {
                let child_repo_file = child.clone().downcast::<ostree::RepoFile>().unwrap();

                // Check if this directory has its own user.component xattr
                let dir_component = get_user_component_xattr(&child_repo_file)?;
                let effective_component = dir_component.or_else(|| parent_component.clone());

                // Recursively process the directory with the new parent component
                build_fs_mapping_recurse(path, &child, state, effective_component)?;
            }
            o => anyhow::bail!("Unhandled file type: {o:?}"),
        }
        path.pop();
    }
    Ok(())
}

fn get_user_component_xattr(file: &ostree::RepoFile) -> std::io::Result<Option<String>> {
    let xattrs = match file.xattrs(gio::Cancellable::NONE) {
        Ok(xattrs) => xattrs,
        Err(_) => return Ok(None), // No xattrs available
    };

    let n = xattrs.n_children();
    for i in 0..n {
        let child = xattrs.child_value(i);
        let key = child.child_value(0);
        let key_bytes = key.data_as_bytes();

        if key_bytes == COMPONENT_XATTR.to_bytes_with_nul() {
            let value = child.child_value(1);
            let value = value.data_as_bytes();
            let value_str = String::from_utf8(value.to_vec())
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            return Ok(Some(value_str));
        }
    }
    Ok(None)
}

///This is ostree-ext encapsulate but its using chunks from packages
pub(crate) fn container_encapsulate(args: Vec<String>) -> Result<()> {
    use pacman_manager::read_packages_from_commit;

    // Parse CLI arguments
    let opt = ContainerEncapsulateOpts::parse_from(&args[1..]);

    let repo = &ostree_ext::cli::parse_repo(&opt.repo)?;
    let (root, rev) = repo.read_commit(opt.ostree_ref.as_str(), gio::Cancellable::NONE)?;
    let cancellable = gio::Cancellable::new();

    let mut state = MappingBuilder {
        unpackaged_id: Rc::from(MappingBuilder::UNPACKAGED_ID),
        packagemeta: Default::default(),
        componentmeta: Default::default(),
        checksum_paths: Default::default(),
        path_packages: Default::default(),
        path_components: Default::default(),
        skip: Default::default(),
        component_ids: Default::default(),
        pkgsize: Default::default(),
    };

    // Insert metadata for unpackaged content
    state.packagemeta.insert(ObjectSourceMeta {
        identifier: Rc::clone(&state.unpackaged_id),
        name: Rc::clone(&state.unpackaged_id),
        srcid: Rc::clone(&state.unpackaged_id),
        change_time_offset: u32::MAX,
        change_frequency: u32::MAX,
    });

    // Load Pacman packages from commit
    let package_meta = read_packages_from_commit(&opt.repo, &opt.ostree_ref)
        .context("Reading Pacman package metadata")?;

    if package_meta.is_empty() {
        return Err(anyhow::anyhow!("Failed to find any Pacman packages").into());
    }

    let mut lowest_change_time: Option<(Rc<str>, u64)> = None;
    let mut highest_change_time: Option<u64> = None;

    // Walk packages
    for pkgmeta in package_meta.values() {
        let nevra = Rc::from(format!("{}-{}.{}", pkgmeta.pkgname, pkgmeta.pkgver, pkgmeta.arch).into_boxed_str());

        if let Some((lowid, lowtime)) = lowest_change_time.as_mut() {
            if *lowtime > pkgmeta.buildtime {
                *lowid = Rc::clone(&nevra);
                *lowtime = pkgmeta.buildtime;
            }
        } else {
            lowest_change_time = Some((Rc::clone(&nevra), pkgmeta.buildtime));
        }

        if let Some(hightime) = highest_change_time.as_mut() {
            if *hightime < pkgmeta.buildtime {
                *hightime = pkgmeta.buildtime;
            }
        } else {
            highest_change_time = Some(pkgmeta.buildtime);
        }

        state.pkgsize += pkgmeta.size;

        // Insert package metadata
        state.packagemeta.insert(ObjectSourceMeta {
            identifier: Rc::clone(&nevra),
            name: Rc::from(pkgmeta.pkgname.clone()),
            srcid: Rc::from(pkgmeta.src_pkg.clone()),
            change_time_offset: 0, // compute later
            change_frequency: pkgmeta.changelogs.len() as u32,
        });

        // Map provided files
        for path in &pkgmeta.provided_files {
            state.path_packages.entry(path.clone()).or_default().insert(Rc::clone(&nevra));
        }
    }

    let (lowest_change_name, lowest_change_time) =
        lowest_change_time.expect("Failed to find any packages");
    let highest_change_time = highest_change_time.expect("Failed to find any packages");

    // Compute offsets
    for pkgmeta in package_meta.values() {
        // Build a NEVRA-like string
        let nevra_str = format!("{}-{}.{}", 
            pkgmeta.pkgname, 
            pkgmeta.pkgver,
            pkgmeta.arch
        );
        let nevra: Rc<str> = Rc::from(nevra_str.into_boxed_str());

        let change_time_offset = ((pkgmeta.buildtime - lowest_change_time) / 3600) as u32;

        // Insert into HashSet<ObjectSourceMeta>
        state.packagemeta.insert(ObjectSourceMeta {
            identifier: Rc::clone(&nevra),
            name: Rc::from(pkgmeta.pkgname.clone()),
            srcid: Rc::from(pkgmeta.src_pkg.clone()),
            change_time_offset,
            change_frequency: pkgmeta.changelogs.len() as u32,
        });
    }


    // Kernel and initramfs
    if let Some(kernel_dir) = ostree_ext::bootabletree::find_kernel_dir(&root, gio::Cancellable::NONE)? {
        let kernel_ver: Utf8PathBuf = kernel_dir.basename().unwrap().try_into().map_err(anyhow::Error::msg)?;
        let initramfs = kernel_dir.child("initramfs.img");
        if initramfs.query_exists(gio::Cancellable::NONE) {
            let path: Utf8PathBuf = initramfs.path().unwrap().try_into().map_err(anyhow::Error::msg)?;
            let initramfs = initramfs.downcast_ref::<ostree::RepoFile>().unwrap();
            let checksum = initramfs.checksum();
            let name = "initramfs".to_string();
            let identifier = Rc::from(format!("{} (kernel {})", name, kernel_ver).into_boxed_str());

            state.checksum_paths.entry(checksum.to_string()).or_default().insert(path.clone());
            state.path_packages.entry(path.clone()).or_default().insert(Rc::clone(&identifier));
            state.packagemeta.insert(ObjectSourceMeta {
                identifier: Rc::clone(&identifier),
                name: Rc::from(name),
                srcid: Rc::clone(&identifier),
                change_time_offset: u32::MAX,
                change_frequency: u32::MAX,
            });
            state.skip.insert(path);
        }
    }

    // Walk filesystem (without progress_task)
    build_fs_mapping_recurse(&mut Utf8PathBuf::from("/"), &root, &mut state, None)?;

    // Component metadata
    for component_name in state.component_ids.iter() {
        let component_id = Rc::from(component_name.clone());
        let component_srcid = Rc::from(format!("component:{}", component_name));

        state.componentmeta.insert(ObjectSourceMeta {
            identifier: component_id,
            name: Rc::from(component_name.clone()),
            srcid: component_srcid,
            change_time_offset: u32::MAX,
            change_frequency: u32::MAX,
        });
    }

    let src_pkgs: std::collections::HashSet<_> = state.packagemeta.iter().map(|p| &p.srcid).collect();

    println!(
        "{} objects in {} packages ({} source)",
        state.checksum_paths.len(),
        state.packagemeta.len(),
        src_pkgs.len(),
    );
    println!("pacman size: {}", state.pkgsize);
    println!(
        "Earliest changed package: {} at {}",
        lowest_change_name,
        chrono::Utc.timestamp_opt(lowest_change_time.try_into().unwrap(), 0).unwrap()
    );

    let (package_meta, component_content_map) = state.create_meta();
    let package_meta_sized = ObjectMetaSized::compute_sizes(repo, package_meta)?;

    if let Some(v) = opt.write_contentmeta_json {
        let v = v.strip_prefix("/").unwrap_or(&v);
        let root_dir = cap_std::fs::Dir::open_ambient_dir("/", cap_std::ambient_authority())?;
        root_dir.atomic_replace_with(v, |w| {
            serde_json::to_writer(w, &package_meta_sized.sizes).map_err(anyhow::Error::msg)
        })?;
    }

    // Container export
    let labels = opt
        .labels
        .into_iter()
        .filter_map(|l| l.split_once('=').map(|(k,v)| (k.to_string(), v.to_string())))
        .collect::<std::collections::BTreeMap<_,_>>();

    let package_structure = opt.previous_build_manifest.as_ref().map(|p| {
        oci_spec::image::ImageManifest::from_file(p)
            .map_err(|e| anyhow::anyhow!("Failed to read previous manifest {p}: {e}"))
    }).transpose()?;

    let copy_meta_opt_keys = opt
        .copy_meta_opt_keys
        .into_iter()
        .chain(std::iter::once("pacmanostree.inputhash".to_owned()))
        .collect();

    let config = Config { labels: Some(labels), cmd: opt.cmd };
    let mut opts = ExportOpts::default();
    opts.copy_meta_keys = opt.copy_meta_keys;
    opts.copy_meta_opt_keys = copy_meta_opt_keys;
    opts.max_layers = opt.max_layers;
    opts.prior_build = package_structure.as_ref();
    opts.package_contentmeta = Some(&package_meta_sized);
    opts.specific_contentmeta = Some(&component_content_map);

    use ostree_ext::oci_spec::image::Config as OciConfig;

    if let Some(config_path) = opt.image_config.as_deref() {
        let oci_config: OciConfig = serde_json::from_reader(
         File::open(config_path).map(BufReader::new)?
            ).map_err(anyhow::Error::msg)?;
        opts.container_config = Some(oci_config);
    }

    if let Some(arch) = opt.arch.as_ref() {
        let platform = PlatformBuilder::default()
            .architecture(arch.clone())
            .os(Os::default())
            .build()
            .unwrap();
        opts.platform = Some(platform);
    }

    if opt.format_version >= 2 {
        opts.tar_create_parent_dirs = true;
    }

    let handle = tokio::runtime::Handle::current();
    println!("Generating container image");
    let digest = handle.block_on(async {
        ostree_ext::container::encapsulate(repo, rev.as_str(), &config, Some(opts), &opt.imgref)
            .await
            .context("Encapsulating")
    })?;

    println!("Pushed digest: {}", digest);
    Ok(())
}
