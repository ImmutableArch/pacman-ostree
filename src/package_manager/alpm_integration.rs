use alpm::{Alpm, DepMod, Package as AlpmPkg};
use alpm_utils::alpm_with_conf;
use pacmanconf::Config;
use crate::package_solver::{AlpmPool, AlpmPackage, AlpmDep, AlpmProvide};
use anyhow::{Result, Context};
use std::path::Path;

/// Integration with the actual ALPM Pacman database
pub struct AlpmRepository {
    alpm: Alpm,
}

impl AlpmRepository {
    /// Create a new repository with default pacman config
    pub fn new() -> Result<Self> {
        let config = Config::new()
            .context("Failed to load pacman config")?;
        let alpm = alpm_utils::alpm_with_conf(&config)
            .context("Failed to initialize ALPM")?;
        Ok(Self { alpm })
    }

    /// Create a repository from a custom pacman.conf path
    pub fn with_config(config_path: &Path) -> Result<Self> {
        let config = Config::from_file(config_path)
            .context("Failed to load custom pacman config")?;
        let alpm = alpm_utils::alpm_with_conf(&config)
            .context("Failed to initialize ALPM with custom config")?;
        Ok(Self { alpm })
    }

    /// Create a repository from custom pacman.conf path and rootdir
    pub fn with_config_and_rootdir(config_path: &Path, rootdir: &str) -> Result<Self> {
        let mut config = Config::from_file(config_path)
            .context("Failed to load custom pacman config")?;
        config.root_dir = rootdir.to_string();
        config.db_path = format!("{}/var/lib/pacman", rootdir);
        let alpm = alpm_utils::alpm_with_conf(&config)
            .context("Failed to initialize ALPM with custom rootdir")?;
        Ok(Self { alpm })
    }

    /// Load only sync DB into the pool
    pub fn load_to_pool(&self) -> Result<AlpmPool> {
        self.load_sync_to_pool()
    }

    /// Load sync + local DB into the pool
    pub fn load_all_to_pool(&self) -> Result<AlpmPool> {
        let mut pool = AlpmPool::new();

        for db in self.alpm.syncdbs() {
            for pkg in db.pkgs() {
                let alpm_pkg = self.convert_package(&pkg, db.name())?;
                pool.add_package(alpm_pkg);
            }
        }

        for pkg in self.alpm.localdb().pkgs() {
            let alpm_pkg = self.convert_package(&pkg, "local")?;
            pool.add_package(alpm_pkg);
        }

        pool.finalize_virtuals();
        Ok(pool)
    }

    /// Load only sync DB into the pool
    pub fn load_sync_to_pool(&self) -> Result<AlpmPool> {
        let mut pool = AlpmPool::new();

        for db in self.alpm.syncdbs() {
            for pkg in db.pkgs() {
                let alpm_pkg = self.convert_package(&pkg, db.name())?;
                pool.add_package(alpm_pkg);
            }
        }

        pool.finalize_virtuals();
        Ok(pool)
    }

    /// Load only local DB into the pool
    pub fn load_local_to_pool(&self) -> Result<AlpmPool> {
        let mut pool = AlpmPool::new();

        for pkg in self.alpm.localdb().pkgs() {
            let alpm_pkg = self.convert_package(&pkg, "local")?;
            pool.add_package(alpm_pkg);
        }

        pool.finalize_virtuals();
        Ok(pool)
    }

    /// Download packages to cache using ALPM fetch_pkgurl
    pub fn download_packages_to_cache(
        &mut self,
        pkg_names: &[String],
        cache_dir: &str,
    ) -> Result<()> {
        std::fs::create_dir_all(cache_dir)?;
        self.alpm.set_cachedirs([cache_dir].iter())
            .map_err(|e| anyhow::anyhow!("Failed to set cache directories: {}", e))?;

        use alpm::AlpmListMut;
        let mut urls: Vec<String> = Vec::new();

        for pkg_name in pkg_names {
            let mut found = false;
            for db in self.alpm.syncdbs() {
                if let Ok(pkg) = db.pkg(pkg_name.as_str()) {
                    if let Some(filename) = pkg.filename() {
                        if let Some(server) = db.servers().iter().next() {
                            let url = if server.ends_with('/') {
                                format!("{}{}", server, filename)
                            } else {
                                format!("{}/{}", server, filename)
                            };
                            urls.push(url);
                            found = true;
                            break;
                        }
                    }
                }
            }
            if !found {
                println!("Skipped {} - no URL found", pkg_name);
            }
        }

        if urls.is_empty() {
            return Ok(());
        }

        let mut url_list: AlpmListMut<String> = AlpmListMut::new();
        for url in &urls {
            url_list.push(url.clone());
        }

        let fetched = self.alpm.fetch_pkgurl(url_list)?;
        println!("Downloaded {} packages", fetched.len());

        Ok(())
    }

    /// Find a package in sync DB
    pub fn find_package(&self, name: &str) -> Result<Option<AlpmPackage>> {
        for db in self.alpm.syncdbs() {
            if let Ok(pkg) = db.pkg(name) {
                return Ok(Some(self.convert_package(&pkg, db.name())?));
            }
        }
        Ok(None)
    }

    /// Get package file path
    pub fn get_package_file_path(&self, name: &str) -> Result<Option<String>> {
        for db in self.alpm.syncdbs() {
            if let Ok(pkg) = db.pkg(name) {
                if let Some(filename) = pkg.filename() {
                    return Ok(Some(filename.to_string()));
                }
            }
        }
        Ok(None)
    }

    /// Expand group or package names into a list of packages
    pub fn expand_names(&self, names: Vec<&str>) -> Result<Vec<String>> {
        let mut expanded = Vec::new();

        for name in names {
            let mut found = false;

            'db_loop: for db in self.alpm.syncdbs() {
                let mut group_packages = Vec::new();
                for pkg in db.pkgs() {
                    if pkg.groups().iter().any(|g| g == name) {
                        group_packages.push(pkg.name().to_string());
                    }
                }

                if !group_packages.is_empty() {
                    for pkg_name in group_packages {
                        expanded.push(pkg_name);
                    }
                    found = true;
                    break 'db_loop;
                }
            }

            if !found {
                if self.find_package(name)?.is_some() {
                    expanded.push(name.to_string());
                    found = true;
                }
            }

            if !found {
                anyhow::bail!("Package or group '{}' not found", name);
            }
        }

        Ok(expanded)
    }

    /// Get repository names
    pub fn get_repos(&self) -> Vec<String> {
        self.alpm.syncdbs().iter().map(|db| db.name().to_string()).collect()
    }

    /// Database stats
    pub fn get_stats(&self) -> String {
        let dbs = self.alpm.syncdbs();
        let mut total = 0;
        let mut info = Vec::new();
        for db in dbs {
            let count = db.pkgs().len();
            total += count;
            info.push(format!("{}: {} packages", db.name(), count));
        }
        format!("Total repositories: {}, Total packages: {}\n{}", info.len(), total, info.join("\n"))
    }

    // ── Package conversion ──────────────────────────────────────────────────

    fn convert_dep(dep: &alpm::Dep) -> AlpmDep {
        let name = dep.name().to_string();
        let constraint = match dep.version() {
            None => String::new(),
            Some(ver) => {
                let op = match dep.depmod() {
                    DepMod::Any => return AlpmDep { name, constraint: String::new() },
                    DepMod::Eq  => "=",
                    DepMod::Ge  => ">=",
                    DepMod::Le  => "<=",
                    DepMod::Gt  => ">",
                    DepMod::Lt  => "<",
                };
                format!("{} {}", op, ver.as_str())
            }
        };
        AlpmDep { name, constraint }
    }

    fn convert_package(&self, pkg: &AlpmPkg, repo: &str) -> Result<AlpmPackage> {
        let deps = pkg.depends().iter().map(|d| Self::convert_dep(d)).collect();
        let provides = pkg.provides().iter().map(|p| {
            AlpmProvide {
                virtual_name: p.name().to_string(),
                virtual_version: p.version().map(|v| v.as_str().to_string()).unwrap_or_else(|| "0".to_string()),
            }
        }).collect();
        let conflicts = pkg.conflicts().iter().map(|c| c.name().to_string()).collect();
        let size = pkg.size().max(0) as u64;

        let full_version = pkg.version().as_str();
        let (version, pkgrel) = match full_version.rfind('-') {
            Some(pos) => (full_version[..pos].to_string(), full_version[pos+1..].to_string()),
            None => (full_version.to_string(), "1".to_string()),
        };

        Ok(AlpmPackage {
            name: pkg.name().to_string(),
            version,
            pkgrel,
            repo: repo.to_string(),
            size,
            deps,
            provides,
            conflicts,
        })
    }
}

impl Default for AlpmRepository {
    fn default() -> Self {
        Self::new().expect("Failed to initialize ALPM Repository")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_repository_creation() {
        if let Ok(repo) = AlpmRepository::new() {
            let stats = repo.get_stats();
            println!("{}", stats);
            assert!(stats.contains("Total repositories:"));
        }
    }
}