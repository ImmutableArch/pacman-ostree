use anyhow::{Context, Result};
use std::num::NonZeroUsize;
use std::path::Path;
use std::process::Command;
use std::ffi::OsStr;
use libc;

use ostree_ext::{gio, glib};

// ===== ENUM =====

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BubblewrapMutability {
    Immutable,
    ReadOnly,
    MutateFreely,
}

// ===== CONST =====

static PATH_VAR: &str = "PATH=/usr/sbin:/usr/bin";

static ADDED_CAPABILITIES: &[&str] = &[
    "cap_chown",
    "cap_dac_override",
    "cap_fowner",
    "cap_fsetid",
    "cap_kill",
    "cap_setgid",
    "cap_setuid",
    "cap_setpcap",
    "cap_sys_chroot",
    "cap_setfcap",
];

static RO_BINDS: &[&str] = &[
    "/sys/block",
    "/sys/bus",
    "/sys/class",
    "/sys/dev",
    "/sys/devices",
];

// ===== STRUCT =====

pub struct Bubblewrap {
    rootfs: String,
    argv: Vec<String>,
    executed: bool,
    child_argv0: Option<NonZeroUsize>,
    launcher: gio::SubprocessLauncher,
}

// ===== HELPERS =====

fn running_in_nspawn() -> bool {
    std::env::var("container").ok().as_deref() == Some("systemd-nspawn")
}

// ===== IMPL =====

impl Bubblewrap {
    pub fn new(rootfs: impl AsRef<Path>) -> Result<Self> {
        let rootfs = rootfs.as_ref().to_string_lossy().to_string();

        let launcher = gio::SubprocessLauncher::new(gio::SubprocessFlags::NONE);

        // ustawienie katalogu roboczego kontenera
        let rootfs_clone = rootfs.clone();
        launcher.set_child_setup(move || {
            std::env::set_current_dir(&rootfs_clone).expect("chdir");
        });

        launcher.set_environ(&[Path::new(PATH_VAR).into()]);

        let mut argv = vec![
            "bwrap",
            "--dev", "/dev",
            "--proc", "/proc",
            "--dir", "/run",
            "--dir", "/tmp",
            "--chdir", "/",
            "--die-with-parent",
            "--unshare-pid",
            "--unshare-uts",
            "--unshare-ipc",
            "--unshare-cgroup-try",
        ];

        for d in RO_BINDS {
            argv.push("--ro-bind");
            argv.push(d);
            argv.push(d);
        }

        if !running_in_nspawn() {
            argv.push("--unshare-net");
        }

        if nix::unistd::Uid::effective().is_root() {
            argv.extend(&["--cap-drop", "ALL"]);
            for cap in ADDED_CAPABILITIES {
                argv.push("--cap-add");
                argv.push(cap);
            }
        }

        Ok(Self {
            rootfs,
            argv: argv.into_iter().map(|s| s.to_string()).collect(),
            executed: false,
            child_argv0: None,
            launcher,
        })
    }

    pub fn new_with_mutability(
        rootfs: impl AsRef<Path>,
        mutability: BubblewrapMutability,
    ) -> Result<Self> {
        let mut b = Self::new(rootfs)?;

        match mutability {
            BubblewrapMutability::Immutable | BubblewrapMutability::ReadOnly => {
                b.bind_read("usr", "/usr");
                b.bind_read("etc", "/etc");
            }
            BubblewrapMutability::MutateFreely => {
                b.bind_readwrite("usr", "/usr");
                b.bind_readwrite("etc", "/etc");
            }
        }

        Ok(b)
    }

    // ===== ARGS =====

    pub fn append_child_argv<'a>(&mut self, args: impl IntoIterator<Item = &'a str>) {
        if self.child_argv0.is_none() {
            self.child_argv0 = Some(self.argv.len().try_into().unwrap());
        }
        self.argv.extend(args.into_iter().map(|s| s.to_string()));
    }

    pub fn bind_read(&mut self, src: &str, dest: &str) {
        self.argv.extend(["--ro-bind", src, dest].iter().map(|s| s.to_string()));
    }

    pub fn bind_readwrite(&mut self, src: &str, dest: &str) {
        self.argv.extend(["--bind", src, dest].iter().map(|s| s.to_string()));
    }

    pub fn setenv(&mut self, key: &str, val: &str) {
        self.launcher.setenv(key, val, true);
    }

    pub fn prepend_rootfs_bind(&mut self, src: &str, dest: &str) {
        // Wstaw --bind src dest na początku argv (przed --dev, --proc itd.)
        let insert_args = vec![
            "--bind".to_string(),
            src.to_string(),
            dest.to_string(),
        ];
        // Wstaw na pozycji 0 (po "bwrap" które jest na [0])
        // Ale argv[0] to "bwrap", więc wstawiamy od indeksu 1
        let mut new_argv = vec![self.argv[0].clone()];
        new_argv.extend(insert_args);
        new_argv.extend(self.argv[1..].iter().cloned());
        self.argv = new_argv;

        // Zaktualizuj child_argv0 jeśli był ustawiony
        if let Some(idx) = self.child_argv0 {
            self.child_argv0 = Some(NonZeroUsize::new(idx.get() + 3).unwrap());
        }
    }

    // ===== RUN =====

    pub fn spawn(&mut self) -> Result<(gio::Subprocess, String)> {
        if self.executed {
            anyhow::bail!("Already executed");
        }
        self.executed = true;

        let idx = self.child_argv0.map(|x| x.get()).unwrap_or(0);
        let argv0 = format!("bwrap({})", self.argv.get(idx).unwrap_or(&"?".into()));

        let argv: Vec<&OsStr> = self.argv.iter().map(|s| OsStr::new(s)).collect();
        let child = self.launcher.spawn(&argv)?;

        Ok((child, argv0))
    }

    pub fn run(&mut self) -> Result<()> {
        let (child, name) = self.spawn()?;
        child.wait_check(None::<&gio::Cancellable>)?;
        Ok(())
    }

    pub fn run_captured(&mut self) -> Result<Vec<u8>> {
        self.launcher.set_flags(gio::SubprocessFlags::STDOUT_PIPE);

        let (child, name) = self.spawn()?;
        let (stdout, _stderr) = child.communicate(None::<&glib::Bytes>, None::<&gio::Cancellable>)?;

        child.wait_check(None::<&gio::Cancellable>).context(name)?;

        let stdout = stdout.expect("no stdout");
        Ok(stdout.to_vec())
    }

    pub fn run_with_stdin(&mut self, input: &[u8]) -> Result<()> {
        self.launcher.set_flags(gio::SubprocessFlags::STDIN_PIPE);
        let (child, name) = self.spawn()?;
        child.communicate(
        Some(&glib::Bytes::from(input)),
        None::<&gio::Cancellable>,
        )?;
        child.wait_check(None::<&gio::Cancellable>).context(name)?;
        Ok(())
    }
}


// ===== PROSTA FUNKCJA =====

pub fn bubblewrap_run(
    rootfs: &str,
    args: &[&str],
    mutability: BubblewrapMutability,
) -> Result<Vec<u8>> {
    let mut bwrap = Bubblewrap::new_with_mutability(rootfs, mutability)?;

    if mutability == BubblewrapMutability::MutateFreely {
        bwrap.bind_readwrite("var", "/var");
    } else {
        bwrap.bind_read("var", "/var");
    }

    bwrap.append_child_argv(args.iter().copied());

    bwrap.run_captured()
}