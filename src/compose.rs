//Build Arch-Based OSTree OCI Image
//TODO - 1. YAML Config Parser 2. Preparation logic 3. Rootfs Preparation 4. OSTREE Commit

use std::error::Error;
use tokio::io::AsyncReadExt;
use serde::Deserialize;
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::num::NonZeroU32;
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

const SYSROOT: &str = "sysroot";
const USR: &str = "usr";
const ETC: &str = "etc";
const OCI_ARCHIVE_TRANSPORT: &str = "oci-archive";
