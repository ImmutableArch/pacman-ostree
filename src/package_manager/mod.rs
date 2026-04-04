pub mod config;
pub mod installer;
pub mod alpm_integration;

pub use installer::PackageManager;
pub use config::{PacmanConfig};
pub use alpm_integration::AlpmRepository;

use serde::{Deserialize, Serialize};

/// Reprezentacja pakietu w formacie: name-version-pkgrel@repo
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub pkgrel: String,
    pub repo: String,
}

impl Package {
    pub fn new(name: String, version: String, pkgrel: String, repo: String) -> Self {
        Self { name, version, pkgrel, repo }
    }

    /// Format: name-version-pkgrel@repo
    pub fn full_name(&self) -> String {
        format!("{}-{}-{}@{}", self.name, self.version, self.pkgrel, self.repo)
    }

    /// Format: name-version-pkgrel
    pub fn display_name(&self) -> String {
        format!("{}-{}-{}", self.name, self.version, self.pkgrel)
    }
}

impl std::fmt::Display for Package {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.full_name())
    }
}
