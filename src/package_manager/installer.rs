use crate::package_solver::AlpmPool;
use crate::package_solver::AlpmDependencyProvider;
use resolvo::{ConditionalRequirement, Problem, Requirement, Solver, UnsolvableOrCancelled};
use super::Package;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::rc::Rc;
use resolvo::NameId;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageInfo {
    pub package: Package,
    pub reason: InstallReason,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum InstallReason {
    Explicit,
    AsDependency,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallResult {
    pub packages: Vec<PackageInfo>,
    pub total_size: u64,
    pub success: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UninstallResult {
    pub packages: Vec<PackageInfo>,
    pub freed_size: u64,
    pub success: bool,
}

pub struct PackageManager {
    pool: Rc<AlpmPool>,
}

impl PackageManager {
    pub fn new(pool: AlpmPool) -> Self {
        Self { pool: Rc::new(pool) }
    }

    pub async fn plan_install(&self, package_names: Vec<&str>) -> Result<InstallResult> {
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

        let provider = AlpmDependencyProvider::new(Rc::clone(&self.pool));

        if let Err((name_a, name_b, _msg)) = provider.validate_requirements(&requirement_ids) {
            return Err(anyhow::anyhow!(
                "Conflict detected: {} - {}",
                provider.pool().resolve_name(name_a),
                provider.pool().resolve_name(name_b)
            ));
        }

        let mut requirements: Vec<ConditionalRequirement> = Vec::new();
        for name_id in &requirement_ids {
            let vs = self.pool.intern_version_set(*name_id, ">= 0");
            requirements.push(Requirement::Single(vs).into());
        }

        let problem = Problem::new().requirements(requirements);
        let mut solver = Solver::new(provider);

        match solver.solve(problem) {
            Ok(solution) => {
                let mut result_packages = Vec::new();
                let mut total_size = 0u64;

                for solvable_id in solution {
                    let solvable = self.pool.resolve_solvable(solvable_id);

                    // Skip already installed packages
                    if solvable.repo == "local" {
                        continue;
                    }

                    let is_explicit = explicit_packages
                        .iter()
                        .any(|&name| name == solvable.name);

                    let pkg_size = self.pool.get_package_size(solvable_id).unwrap_or(0);
                    let pkg_rel = self.pool.get_package_pkgrel(solvable_id)
                        .unwrap_or_else(|| "1".to_string());


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
            Err(UnsolvableOrCancelled::Unsolvable(conflict)) => {
                let display = conflict.display_user_friendly(&solver);
                Err(anyhow::anyhow!(
                    "Cannot resolve dependencies or there is already installed package:\n{}",
                    display
                ))
            }
            Err(UnsolvableOrCancelled::Cancelled(_)) => {
                Err(anyhow::anyhow!("Resolution was cancelled"))
            }
        }
    }

    pub async fn plan_uninstall(&self, package_names: Vec<&str>) -> Result<UninstallResult> {
        self.check_reverse_dependencies(&package_names)?;

        let mut result_packages = Vec::new();
        let mut freed_size = 0u64;

        for name in package_names {
            if let Some(name_id) = self.pool.lookup_name(name) {
                if let Some(candidates) = self.pool.get_candidates_for(name_id) {
                    if let Some(&solvable_id) = candidates.candidates.first() {
                        let solvable = self.pool.resolve_solvable(solvable_id);
                        let pkg_size = self.pool.get_package_size(solvable_id).unwrap_or(0);
                        let pkg_rel = self.pool.get_package_pkgrel(solvable_id)
                            .unwrap_or_else(|| "1".to_string());

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

    fn check_reverse_dependencies(&self, packages_to_remove: &[&str]) -> Result<()> {
        let mut blockers = Vec::new();
        let installed = self.pool.get_installed_packages();

        for (solvable_id, solvable) in installed {
            if packages_to_remove.contains(&solvable.name.as_str()) {
                continue;
            }

            let deps = self.pool.get_deps(solvable_id);

            for dep in deps {
                if packages_to_remove.iter().any(|&rem| rem == dep.name) {
                    blockers.push(format!(
                        "{}-{} (depends on {})",
                        solvable.name, solvable.version, dep.name
                    ));
                }
            }
        }

        if !blockers.is_empty() {
            return Err(anyhow::anyhow!(
                "Cannot uninstall - the following packages depend on them:\n  {}",
                blockers.join("\n  ")
            ));
        }

        Ok(())
    }

    pub fn display_install_plan(result: &InstallResult) {
        println!("\nInstallation Plan");
        println!("{}", "─".repeat(60));

        println!("Packages to install: {}", result.packages.len());

        for p in &result.packages {
            println!(
                "  {}-{} ({})",
                p.package.name,
                p.package.version,
                p.package.repo
            );
        }

        let size_mb = result.total_size / (1024 * 1024);
        println!("\nTotal size: ~{} MB", size_mb);
        println!("{}", "─".repeat(60));
    }

    pub fn display_uninstall_plan(result: &UninstallResult) {
        println!("\nUninstall Plan");
        println!("{}", "─".repeat(60));

        println!("Packages to remove: {}", result.packages.len());

        for p in &result.packages {
            println!(
                "  {}-{} ({})",
                p.package.name,
                p.package.version,
                p.package.repo
            );
        }

        let size_mb = result.freed_size / (1024 * 1024);
        println!("\nFreed space: ~{} MB", size_mb);
        println!("{}", "─".repeat(60));
    }
}