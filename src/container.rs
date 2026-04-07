use ostree_ext::{bootabletree, gio, glib, ostree};
use glib::prelude::*;
use ostree_ext::chunking::ObjectMetaSized;
use ostree_ext::container::{Config, ExportOpts, ImageReference};
use ostree_ext::containers_image_proxy;
use ostree_ext::objectsource::{
    ContentID, ObjectMeta, ObjectMetaMap, ObjectMetaSet, ObjectSourceMeta,
};
use ostree_ext::oci_spec::image::{Arch, Os, PlatformBuilder};
use ostree_ext::prelude::*;
use ostree_ext::oci_spec;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use camino::{Utf8Path, Utf8PathBuf};
use std::rc::Rc;
use anyhow::{Context, Result};
use std::ffi::CStr;
use clap::Parser;
use std::str::FromStr;
use alpm_db::desc::DbDescFileV1;
use alpm_db::files::DbFilesV1;
use std::num::NonZeroU32;
use crate::fsutil::ResolvedOstreePaths;
use cap_std::fs_utf8::Dir;
use std::fs::File;
use std::io::BufReader;
use cap_std_ext::dirext::CapStdExtDirExtUtf8;
use crate::fsutil::FileHelpers;


const COMPONENT_XATTR: &CStr = c"user.component";

#[derive(Debug, Parser)]
pub struct ContainerEncapsulateOpts {
    #[clap(long)]
    #[clap(value_parser)]
    pub repo: Utf8PathBuf,
    pub ostree_ref: String,
    #[clap(value_parser = ostree_ext::cli::parse_base_imgref)]
    pub imgref: ImageReference,
    #[clap(name = "label", long, short)]
    pub labels: Vec<String>,
    #[clap(long)]
    pub image_config: Option<Utf8PathBuf>,
    #[clap(long)]
    pub arch: Option<Arch>,
    #[clap(name = "copymeta", long)]
    pub copy_meta_keys: Vec<String>,
    #[clap(name = "copymeta-opt", long)]
    pub copy_meta_opt_keys: Vec<String>,
    #[clap(long)]
    pub cmd: Option<Vec<String>>,
    #[clap(long)]
    pub max_layers: Option<NonZeroU32>,
    #[clap(long, default_value = "1")]
    pub format_version: u32,
    #[clap(long)]
    pub write_contentmeta_json: Option<Utf8PathBuf>,
    #[clap(name = "compare-with-build", long)]
    pub compare_with_build: Option<String>,
    #[clap(long)]
    pub previous_build_manifest: Option<Utf8PathBuf>,
}

#[derive(Debug)]
struct MappingBuilder {
    packagemeta: ObjectMetaSet,
    componentmeta: ObjectMetaSet,
    component_ids: HashSet<String>,
    checksum_paths: BTreeMap<String, BTreeSet<Utf8PathBuf>>,
    path_packages: HashMap<Utf8PathBuf, BTreeSet<ContentID>>,
    path_components: HashMap<Utf8PathBuf, BTreeSet<ContentID>>,
    unpackaged_id: ContentID,
    skip: HashSet<Utf8PathBuf>,
    pacmanSize: u64,
}

impl MappingBuilder {
    const UNPACKAGED_ID: &'static str = "rpmostree-unpackaged-content";

    fn duplicate_objects(&self) -> impl Iterator<Item = (&String, &BTreeSet<Utf8PathBuf>)> {
        self.checksum_paths.iter().filter(|(_, paths)| paths.len() > 1)
    }

    fn multiple_owners(&self) -> impl Iterator<Item = (&Utf8PathBuf, &BTreeSet<ContentID>)> {
        self.path_packages.iter().filter(|(_, pkgs)| pkgs.len() > 1)
    }

    fn create_meta(&self) -> (ObjectMeta, BTreeMap<ContentID, Vec<(Utf8PathBuf, String)>>) {
        let mut package_content = ObjectMetaMap::default();
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

                if state.skip.remove(Utf8Path::new(path)) {
                    path.pop();
                    continue;
                }

                let file_component = get_user_component_xattr(&child)?;
                let effective_component = file_component.or_else(|| parent_component.clone());

                if let Some(component_name) = effective_component {
                    let component_id = Rc::from(component_name.clone());
                    state.component_ids.insert(component_name);
                    state.path_components.entry(path.clone()).or_default().insert(Rc::clone(&component_id));
                };

                let checksum = child.checksum().to_string();
                state.checksum_paths.entry(checksum).or_default().insert(path.clone());
            }
            gio::FileType::Directory => {
                let child_repo_file = child.clone().downcast::<ostree::RepoFile>().unwrap();
                let dir_component = get_user_component_xattr(&child_repo_file)?;
                let effective_component = dir_component.or_else(|| parent_component.clone());
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
        Ok(x) => x,
        Err(_) => return Ok(None),
    };

    for i in 0..xattrs.n_children() {
        let child = xattrs.child_value(i);
        let key = child.child_value(0);
        let key_bytes = key.data_as_bytes();
        if key_bytes == COMPONENT_XATTR.to_bytes_with_nul() {
            let value = child.child_value(1);
            let value = value.data_as_bytes();
            return Ok(Some(String::from_utf8(value.to_vec())
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?));
        }
    }
    Ok(None)
}

pub async fn container_encapsulate(args: ContainerEncapsulateOpts) -> anyhow::Result<()> {
    use crate::fsutil::FileHelpers; // zapewnia .is_regular() i .is_symlink()
    use anyhow::Context;

    let opt = args;
    let repo = &ostree_ext::cli::parse_repo(&opt.repo)?;
    let (root, _rev) = repo.read_commit(opt.ostree_ref.as_str(), gio::Cancellable::NONE)?;

    let mut state = MappingBuilder {
        unpackaged_id: Rc::from(MappingBuilder::UNPACKAGED_ID),
        packagemeta: Default::default(),
        componentmeta: Default::default(),
        checksum_paths: Default::default(),
        path_packages: Default::default(),
        path_components: Default::default(),
        skip: Default::default(),
        component_ids: Default::default(),
        pacmanSize: 0,
    };

    state.packagemeta.insert(ObjectSourceMeta {
        identifier: Rc::clone(&state.unpackaged_id),
        name: Rc::clone(&state.unpackaged_id),
        srcid: Rc::clone(&state.unpackaged_id),
        change_time_offset: u32::MAX,
        change_frequency: u32::MAX,
    });

    // Wczytaj paczki z bazy
    let db_path = Utf8PathBuf::from("/usr/share/pacman/local");
    let mut package_meta: HashMap<Rc<str>, (DbDescFileV1, Utf8PathBuf)> = HashMap::new();

    for entry in std::fs::read_dir(&db_path)? {
        let entry = entry?;
        let pkg_dir = entry.path();
        let desc_path = pkg_dir.join("desc");
        let files_path = pkg_dir.join("files");
        if !desc_path.exists() || !files_path.exists() {
            continue;
        }

        let desc_data = std::fs::read_to_string(&desc_path)?;
        let desc = DbDescFileV1::from_str(&desc_data)?;
        let nevra = Rc::from(format!("{}-{}", desc.name, desc.version).into_boxed_str());
        state.pacmanSize += desc.size;

        // Konwersja PathBuf → Utf8PathBuf
        let files_utf8 = Utf8PathBuf::from_path_buf(files_path)
            .map_err(|pb| anyhow::anyhow!("Nieprawidłowa ścieżka UTF-8: {:?}", pb))?;
        package_meta.insert(nevra, (desc, files_utf8));
    }

    let mut dir_cache: HashMap<Utf8PathBuf, ResolvedOstreePaths> = HashMap::new();

    // Iteruj pliki paczek
    for (nevra, (_desc, files_path)) in package_meta.iter() {
        // Wczytaj cały plik `files` jako tekst
        let files_data = std::fs::read_to_string(files_path)?;
        // Parsuj jako linie, pomiń nagłówek "%FILES%"
        for line in files_data.lines().skip(1) {
            let rel_path = line.trim();
            if rel_path.is_empty() { continue; }
            // Jeśli to katalog, pomiń
            if rel_path.ends_with('/') { continue; }

            let path = Utf8PathBuf::from("/").join(rel_path);

            if let Some(ostree_paths) = crate::fsutil::resolve_ostree_paths(
                &path,
                root.downcast_ref::<ostree::RepoFile>().unwrap(),
                &mut dir_cache,
            ) {
                // is_regular i is_symlink z trait FileHelpers
                if ostree_paths.path.is_regular() || ostree_paths.path.is_symlink() {
                    // peek_path() może zwrócić PathBuf, więc konwertujemy ręcznie
                    let real_path_utf8 = Utf8PathBuf::from_path_buf(
                        ostree_paths.path.peek_path().unwrap()
                    ).map_err(|pb| anyhow::anyhow!("Nieprawidłowa ścieżka UTF-8: {:?}", pb))?;

                    let checksum = ostree_paths.path.checksum().to_string();

                    state
                        .checksum_paths
                        .entry(checksum.clone())
                        .or_default()
                        .insert(real_path_utf8.clone());

                    state
                        .path_packages
                        .entry(real_path_utf8)
                        .or_default()
                        .insert(Rc::clone(nevra));
                }
            }
        }
    }

    // Reszta funkcji – budowanie map, metadanych komponentów itd.
    build_fs_mapping_recurse(&mut Utf8PathBuf::from("/"), &root, &mut state, None)?;

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

    let (package_meta_obj, component_content_map) = state.create_meta();
    let package_meta_sized = ObjectMetaSized::compute_sizes(repo, package_meta_obj)?;

    if let Some(v) = opt.write_contentmeta_json {
        let v = v.strip_prefix("/").unwrap_or(&v);
        let root_dir = Dir::open_ambient_dir("/", cap_std::ambient_authority())?;
        root_dir.atomic_replace_with(v, |w| {
            serde_json::to_writer(w, &package_meta_sized.sizes).map_err(anyhow::Error::msg)
        })?;
    }

    // Pozostała część konfiguracji kontenera …
    let labels = opt.labels.into_iter()
        .map(|l| {
            let (k, v) = l.split_once('=').ok_or_else(|| anyhow::anyhow!("Missing '=' in label {}", l))?;
            Ok((k.to_string(), v.to_string()))
        })
        .collect::<anyhow::Result<_>>()?;

    let package_structure = opt.previous_build_manifest.as_ref()
        .map(|p| oci_spec::image::ImageManifest::from_file(p)
             .map_err(|e| anyhow::anyhow!("Failed to read previous manifest {}: {}", p, e)))
        .transpose()?;

    let copy_meta_opt_keys = opt.copy_meta_opt_keys.into_iter()
        .chain(std::iter::once("rpmostree.inputhash".to_owned()))
        .collect();

    let config = Config { labels: Some(labels), cmd: opt.cmd };
    let mut opts = ExportOpts::default();
    opts.copy_meta_keys = opt.copy_meta_keys;
    opts.copy_meta_opt_keys = copy_meta_opt_keys;
    opts.max_layers = opt.max_layers;
    opts.prior_build = package_structure.as_ref();
    opts.package_contentmeta = Some(&package_meta_sized);
    opts.specific_contentmeta = Some(&component_content_map);

    if let Some(config_path) = opt.image_config.as_deref() {
        let config_json = serde_json::from_reader(BufReader::new(File::open(config_path)?))?;
        opts.container_config = Some(config_json);
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

    println!("Generating container image");
    let digest = ostree_ext::container::encapsulate(repo, _rev.as_str(), &config, Some(opts), &opt.imgref)
        .await
        .context("Encapsulating")?;

    println!("Pushed digest: {}", digest);
    Ok(())
}