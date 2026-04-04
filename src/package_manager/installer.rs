use crate::package_solver::AlpmPool;
use crate::package_solver::AlpmDependencyProvider;
use resolvo::{ConditionalRequirement, Problem, Requirement, Solver, UnsolvableOrCancelled};
use super::Package;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::rc::Rc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageInfo {
    pub package: Package,
    pub reason: InstallReason,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum InstallReason {
    Explicit,        // Użytkownik jawnie chce zainstalować
    AsDependency,    // Zainstalowany ze względu na zależność
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallResult {
    pub packages: Vec<PackageInfo>,
    pub total_size: u64,  // Estymowana wielkość w bajtach
    pub success: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UninstallResult {
    pub packages: Vec<PackageInfo>,
    pub freed_size: u64,  // Szacunkowa wielkość do zwolnienia
    pub success: bool,
}

/// Główny driver instalacji/odinstalowania pakietów
pub struct PackageManager {
    pool: Rc<AlpmPool>,
}

impl PackageManager {
    /// Utwórz nowy PackageManager z gotową pulą pakietów
    pub fn new(pool: AlpmPool) -> Self {
        Self { pool: Rc::new(pool) }
    }

    /// Rozwiąż zależności dla pakietów do instalacji
    pub async fn plan_install(&self, package_names: Vec<&str>) -> Result<InstallResult> {
        // Zamieniamy nazwy pakietów na NameId
        let mut requirement_ids = Vec::new();
        let mut explicit_packages = Vec::new();

        for name in &package_names {
            if let Some(name_id) = self.pool.lookup_name(name) {
                requirement_ids.push(name_id);
                explicit_packages.push(*name);
            } else {
                return Err(anyhow::anyhow!("Package not found: {}", name));
            }
        }

        // Walidacja konfliktów
        let provider = AlpmDependencyProvider::new(Rc::clone(&self.pool));
        if let Err((name_a, name_b, _msg)) = provider.validate_requirements(&requirement_ids) {
            return Err(anyhow::anyhow!(
                "Conflict detected: {} - {}",
                provider.pool().resolve_name(name_a),
                provider.pool().resolve_name(name_b)
            ));
        }

        // Przygotuj requirements dla solvera
        let mut requirements: Vec<ConditionalRequirement> = Vec::new();
        for name_id in &requirement_ids {
            let vs = self.pool.intern_version_set(*name_id, ">= 0");
            requirements.push(Requirement::Single(vs).into());
        }

        // Uruchom solver
        let problem = Problem::new().requirements(requirements);
        let mut solver = Solver::new(provider);

        match solver.solve(problem) {
            Ok(solution) => {
                let mut result_packages = Vec::new();
                let mut total_size = 0u64;

                for solvable_id in solution {
                    let solvable = self.pool.resolve_solvable(solvable_id);
                    let is_explicit = explicit_packages.iter().any(|&name| name == solvable.name);
                    
                    // Pobierz rozmiar z AlpmPackage
                    let pkg_size = self.pool.get_package_size(solvable_id).unwrap_or(0);
                    // Pobierz pkgrel z AlpmPackage
                    let pkg_rel = self.pool.get_package_pkgrel(solvable_id).unwrap_or_else(|| "1".to_string());
                    
                    let package = Package::new(
                        solvable.name.clone(),
                        solvable.version.clone(),
                        pkg_rel,
                        solvable.repo.clone(),
                    );

                    total_size += pkg_size;

                    result_packages.push(PackageInfo {
                        package,
                        reason: if is_explicit {
                            InstallReason::Explicit
                        } else {
                            InstallReason::AsDependency
                        },
                    });
                }

                Ok(InstallResult {
                    packages: result_packages,
                    total_size,
                    success: true,
                })
            }
            Err(UnsolvableOrCancelled::Unsolvable(_conflict)) => {
                Err(anyhow::anyhow!(
                    "Cannot resolve dependencies: unresolvable conflict detected"
                ))
            }
            Err(UnsolvableOrCancelled::Cancelled(_)) => {
                Err(anyhow::anyhow!("Resolution was cancelled"))
            }
        }
    }

    /// Rozwiąż zależności dla pakietów do odinstalowania
    pub async fn plan_uninstall(&self, package_names: Vec<&str>) -> Result<UninstallResult> {
        // W prosty przypadku - usuwamy tylko to co poproszono
        // W realnym przypadku sprawdzilibyśmy czy inne pakiety zależą od tych
        let mut result_packages = Vec::new();
        let mut freed_size = 0u64;

        for name in package_names {
            if let Some(name_id) = self.pool.lookup_name(name) {
                // Pobierz najnowszą wersję
                if let Some(candidates) = self.pool.get_candidates_for(name_id) {
                    if let Some(&solvable_id) = candidates.candidates.first() {
                        let solvable = self.pool.resolve_solvable(solvable_id);
                        
                        // Pobierz rozmiar pakietu
                        let pkg_size = self.pool.get_package_size(solvable_id).unwrap_or(0);
                        // Pobierz pkgrel
                        let pkg_rel = self.pool.get_package_pkgrel(solvable_id).unwrap_or_else(|| "1".to_string());
                        
                        let package = Package::new(
                            solvable.name.clone(),
                            solvable.version.clone(),
                            pkg_rel,
                            solvable.repo.clone(),
                        );

                        freed_size += pkg_size;

                        result_packages.push(PackageInfo {
                            package,
                            reason: InstallReason::Explicit,
                        });
                    }
                } else {
                    return Err(anyhow::anyhow!("Package not found: {}", name));
                }
            } else {
                return Err(anyhow::anyhow!("Package not found: {}", name));
            }
        }

        Ok(UninstallResult {
            packages: result_packages,
            freed_size,
            success: true,
        })
    }

    /// Pokaż plan instalacji w czytelnym formacie
    pub fn display_install_plan(result: &InstallResult) {
        println!("\n📋 Installation Plan");
        println!("{}", "─".repeat(60));
        
        let explicit: Vec<_> = result.packages.iter()
            .filter(|p| p.reason == InstallReason::Explicit)
            .collect();
        let dependencies: Vec<_> = result.packages.iter()
            .filter(|p| p.reason == InstallReason::AsDependency)
            .collect();

        if !explicit.is_empty() {
            println!("\n📦 To install:");
            for p in explicit {
                println!("  {} [{}]", p.package.display_name(), p.package.repo);
            }
        }

        if !dependencies.is_empty() {
            println!("\n📚 Dependencies:");
            for p in dependencies {
                println!("  {} [{}]", p.package.display_name(), p.package.repo);
            }
        }

        let size_mb = result.total_size / (1024 * 1024);
        println!("\n💾 Total size: ~{}MB", size_mb);
        println!("{}", "─".repeat(60));
    }

    /// Pokaż plan odinstalowania w czytelnym formacie
    pub fn display_uninstall_plan(result: &UninstallResult) {
        println!("\n📋 Uninstall Plan");
        println!("{}", "─".repeat(60));
        
        println!("\n🗑️  To remove:");
        for p in &result.packages {
            println!("  {} [{}]", p.package.display_name(), p.package.repo);
        }

        let size_mb = result.freed_size / (1024 * 1024);
        println!("\n💾 Space to free: ~{}MB", size_mb);
        println!("{}", "─".repeat(60));
    }
}
