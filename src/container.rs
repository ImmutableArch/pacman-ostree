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
use anyhow::{Context, Result, anyhow};
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
    #[clap(long)]
    pub pacman_db_path: Utf8PathBuf,
}

#[derive(Debug)]
struct MappingBuilder {
    /// Metadane każdego pakietu/komponentu — to jest `ObjectMeta.set`.
    /// Każdy ContentID obecny w `map` musi mieć tutaj wpis.
    packagemeta: ObjectMetaSet,
    componentmeta: ObjectMetaSet,
    component_ids: HashSet<String>,
    checksum_paths: BTreeMap<String, BTreeSet<Utf8PathBuf>>,
    /// checksum -> ContentID pakietu (do wypełnienia ObjectMeta.map)
    path_packages: HashMap<Utf8PathBuf, BTreeSet<ContentID>>,
    path_components: HashMap<Utf8PathBuf, BTreeSet<ContentID>>,
    unpackaged_id: ContentID,
    skip: HashSet<Utf8PathBuf>,
    pacman_size: u64,
}

impl MappingBuilder {
    const UNPACKAGED_ID: &'static str = "pacmanostree-unpackaged-content";

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
    use crate::fsutil::FileHelpers;
    use anyhow::Context;

    fn normalize_component(v: Option<String>) -> Option<String> {
        v.and_then(|s| {
            let s = s.trim().to_string();
            if s.is_empty() { None } else { Some(s) }
        })
    }

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
        pacman_size: 0,
    };

    // unpackaged_id musi być w secie — pliki bez właściciela trafiają do tego bucketa
    state.packagemeta.insert(ObjectSourceMeta {
        identifier: Rc::clone(&state.unpackaged_id),
        name: Rc::clone(&state.unpackaged_id),
        srcid: Rc::clone(&state.unpackaged_id),
        change_time_offset: u32::MAX,
        change_frequency: u32::MAX,
    });

    // ───────── PACMAN DB ─────────
    let db_path = opt.pacman_db_path;

    if !db_path.exists() {
        return Err(anyhow!("Pacman DB path missing: {}", db_path));
    }

    // Mapa nevra -> (desc, files_path).
    // Jednocześnie od razu dodajemy każdy pakiet do packagemeta.set,
    // żeby ObjectMetaSized::compute_sizes nie zgłaszał "Failed to find X in content set".
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

        let nevra: Rc<str> = Rc::from(
            format!("{}-{}", desc.name, desc.version).into_boxed_str()
        );

        state.pacman_size += desc.size;

        // Każdy pakiet musi być w packagemeta.set, inaczej compute_sizes zwróci błąd
        // dla każdego pliku przypisanego do tego pakietu w ObjectMeta.map.
        state.packagemeta.insert(ObjectSourceMeta {
            identifier: Rc::clone(&nevra),
            name: Rc::from(desc.name.as_ref()),
            srcid: Rc::clone(&nevra),
            // Pakiety instalowane rzadko zmieniają się razem — niski offset, wysoka częstość
            change_time_offset: 0,
            change_frequency: 1,
        });

        let files_utf8 = Utf8PathBuf::from_path_buf(files_path)
            .map_err(|pb| anyhow!("Invalid UTF-8 path: {:?}", pb))?;

        package_meta.insert(nevra, (desc, files_utf8));
    }

    let mut dir_cache: HashMap<Utf8PathBuf, ResolvedOstreePaths> = HashMap::new();

    // ───────── MAPOWANIE PACZEK ─────────
    for (nevra, (_desc, files_path)) in package_meta.iter() {
        let files_data = std::fs::read_to_string(files_path)?;

        for line in files_data.lines().skip(1) {
            let rel_path = line.trim();
            if rel_path.is_empty() { continue; }
            if rel_path.ends_with('/') { continue; }

            let path = Utf8PathBuf::from("/").join(rel_path);

            if let Some(ostree_paths) = crate::fsutil::resolve_ostree_paths(
                &path,
                root.downcast_ref::<ostree::RepoFile>().unwrap(),
                &mut dir_cache,
            ) {
                if ostree_paths.path.is_regular() || ostree_paths.path.is_symlink() {
                    let checksum = ostree_paths.path.checksum().to_string();

                    state
                        .checksum_paths
                        .entry(checksum.clone())
                        .or_default()
                        .insert(path.clone());

                    state
                        .path_packages
                        .entry(path.clone())
                        .or_default()
                        .insert(Rc::clone(nevra));
                }
            }
        }
    }

    // ───────── SKAN OSTREE ─────────
    fn recurse(
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

                    let file_component = normalize_component(get_user_component_xattr(&child)?);
                    let effective_component = file_component
                        .or_else(|| normalize_component(parent_component.clone()));

                    if let Some(component_name) = effective_component {
                        let component_id = Rc::from(component_name.clone());
                        state.component_ids.insert(component_name);
                        state.path_components
                            .entry(path.clone())
                            .or_default()
                            .insert(Rc::clone(&component_id));
                    }

                    let checksum = child.checksum().to_string();
                    state.checksum_paths.entry(checksum).or_default().insert(path.clone());
                }

                gio::FileType::Directory => {
                    let child_repo_file = child.clone().downcast::<ostree::RepoFile>().unwrap();

                    let dir_component = normalize_component(get_user_component_xattr(&child_repo_file)?);
                    let effective_component = dir_component
                        .or_else(|| normalize_component(parent_component.clone()));

                    recurse(path, &child, state, effective_component)?;
                }

                o => anyhow::bail!("Unhandled file type: {o:?}"),
            }

            path.pop();
        }

        Ok(())
    }

    recurse(&mut Utf8PathBuf::from("/"), &root, &mut state, None)?;

    // ───────── META KOMPONENTÓW ─────────
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

    // ───────── OCI EXPORT ─────────
    let config = Config {
        labels: Some(BTreeMap::new()),
        cmd: opt.cmd,
    };

    let mut opts = ExportOpts::default();
    opts.max_layers = opt.max_layers;
    opts.package_contentmeta = Some(&package_meta_sized);
    opts.specific_contentmeta = Some(&component_content_map);

    println!("Generating container image");

    let digest = ostree_ext::container::encapsulate(
        repo,
        _rev.as_str(),
        &config,
        Some(opts),
        &opt.imgref,
    )
    .await
    .context("Encapsulating")?;

    println!("Pushed digest: {}", digest);
    Ok(())
}