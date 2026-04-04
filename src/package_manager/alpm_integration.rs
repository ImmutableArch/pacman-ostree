use alpm::{Alpm, Db, Package as AlpmPkg};
use alpm_utils::alpm_with_conf;
use pacmanconf::Config;
use crate::package_solver::{AlpmPool, AlpmPackage, AlpmDep, AlpmProvide};
use anyhow::{Result, Context};
use std::path::Path;

/// Integracja z rzeczywistą bazą ALPM pacmana
pub struct AlpmRepository {
    alpm: Alpm,
}

impl AlpmRepository {
    /// Utwórz nowe repository z domyślną konfiguracją pacmana
    pub fn new() -> Result<Self> {
        let config = Config::new()
            .context("Failed to load pacman config")?;

        let alpm = alpm_utils::alpm_with_conf(&config)
            .context("Failed to initialize ALPM")?;

        Ok(Self { alpm })
    }

    /// Utwórz nowe repository z custom ścieżką do pacman.conf
    pub fn with_config(config_path: &Path) -> Result<Self> {
        let config = Config::from_file(config_path)
            .context("Failed to load custom pacman config")?;

        let alpm = alpm_utils::alpm_with_conf(&config)
            .context("Failed to initialize ALPM with custom config")?;

        Ok(Self { alpm })
    }

    /// Pobierz wszystkie dostępne pakiety i załaduj do AlpmPool
    pub fn load_to_pool(&self) -> Result<AlpmPool> {
        let mut pool = AlpmPool::new();

        // Iteruj przez wszystkie dostępne bazy (repos)
        let dbs = self.alpm.syncdbs();
        
        for db in dbs {
            let repo_name = db.name();
            
            // Iteruj przez wszystkie pakiety w każdej bazie
            for pkg in db.pkgs() {
                let alpm_pkg = self.convert_package(&pkg, repo_name)?;
                pool.add_package(alpm_pkg);
            }
        }

        Ok(pool)
    }

    /// Pobierz pakiety niezainstalowane (z repozytoriów)
    pub fn load_sync_to_pool(&self) -> Result<AlpmPool> {
        let mut pool = AlpmPool::new();
        let dbs = self.alpm.syncdbs();

        for db in dbs {
            let repo_name = db.name();
            for pkg in db.pkgs() {
                let alpm_pkg = self.convert_package(&pkg, repo_name)?;
                pool.add_package(alpm_pkg);
            }
        }

        Ok(pool)
    }

    /// Pobierz tylko zainstalowane pakiety
    pub fn load_local_to_pool(&self) -> Result<AlpmPool> {
        let mut pool = AlpmPool::new();
        
        let local_db = self.alpm.localdb();
        for pkg in local_db.pkgs() {
            let alpm_pkg = self.convert_package_local(&pkg)?;
            pool.add_package(alpm_pkg);
        }

        Ok(pool)
    }

    /// Pobierz konkretny pakiet
    pub fn find_package(&self, name: &str) -> Result<Option<AlpmPackage>> {
        let dbs = self.alpm.syncdbs();
        
        for db in dbs {
            if let Ok(pkg) = db.pkg(name) {
                return Ok(Some(self.convert_package(&pkg, db.name())?));
            }
        }

        Ok(None)
    }

    /// Konwertuj pakiet ALPM do wewnętrznej reprezentacji
    fn convert_package(&self, pkg: &AlpmPkg, repo: &str) -> Result<AlpmPackage> {
        // Pobierz zależności
        let deps = pkg.depends()
            .iter()
            .map(|dep| {
                let name = dep.name().to_string();
                let ver_str = dep.version()
                    .map(|v| v.as_str())
                    .unwrap_or("0");
                let constraint = format!(">= {}", ver_str);
                AlpmDep { name, constraint }
            })
            .collect();

        // Pobierz provides
        let provides = pkg.provides()
            .iter()
            .map(|provide| {
                let virtual_name = provide.name().to_string();
                let virtual_version = provide.version()
                    .map(|v| v.as_str().to_string())
                    .unwrap_or_else(|| "1.0".to_string());
                AlpmProvide { virtual_name, virtual_version }
            })
            .collect();

        // Pobierz konflikty
        let conflicts = pkg.conflicts()
            .iter()
            .map(|conflict| conflict.name().to_string())
            .collect();

        // Pobierz rozmiar pakietu (zwraca i64, konwertuj na u64)
        let size = {
            let sz = pkg.size();
            if sz > 0 { sz as u64 } else { 0 }
        };

        // Wydziel pkgrel z wersji (np. "8.7.1-1" -> version="8.7.1", pkgrel="1")
        let full_version = pkg.version().as_str();
        let (version, pkgrel) = if let Some(last_dash) = full_version.rfind('-') {
            let ver = full_version[..last_dash].to_string();
            let rel = full_version[last_dash + 1..].to_string();
            (ver, rel)
        } else {
            (full_version.to_string(), "1".to_string())
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

    /// Konwertuj zainstalowany pakiet (lokalny)
    fn convert_package_local(&self, pkg: &AlpmPkg) -> Result<AlpmPackage> {
        // Zainstalowane pakiety mają repo "local"
        self.convert_package(pkg, "local")
    }

    /// Pobierz informacje o repozytorium
    pub fn get_repos(&self) -> Vec<String> {
        self.alpm.syncdbs()
            .iter()
            .map(|db| db.name().to_string())
            .collect()
    }

    /// Pobierz statystykę bazy
    pub fn get_stats(&self) -> String {
        let dbs = self.alpm.syncdbs();
        let mut total_packages = 0;
        let mut repo_info = Vec::new();

        for db in dbs {
            let count = db.pkgs().len();
            total_packages += count;
            repo_info.push(format!("{}: {} packages", db.name(), count));
        }

        format!(
            "Total repositories: {}, Total packages: {}\n{}",
            dbs.len(),
            total_packages,
            repo_info.join("\n")
        )
    }
}

impl Default for AlpmRepository {
    fn default() -> Self {
        Self::new().expect("Failed to initialize ALPM Repository - is pacman properly configured?")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_repository_creation() {
        // To test będzie działać na systemie z zainstalowanym pacmanem
        if let Ok(repo) = AlpmRepository::new() {
            let stats = repo.get_stats();
            println!("{}", stats);
            assert!(stats.contains("Total repositories:"));
        }
    }
}
