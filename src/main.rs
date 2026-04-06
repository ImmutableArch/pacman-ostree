mod package_manager;
mod package_solver;

use package_solver::{AlpmPool, AlpmPackage, AlpmDep, AlpmProvide};
use package_manager::{AlpmRepository, PackageManager};
mod package_installer;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("╔════════════════════════════════════════════════════════╗");
    println!("║       ALPM Dependency Resolver + Package Manager       ║");
    println!("╚════════════════════════════════════════════════════════╝\n");

    // Test 1: Example repo
    test_case_1_example().await?;
    
    println!("\n{}", "─".repeat(60));
    println!();
    
    // Test 2: Real ALPM
    test_case_2_real_repo().await.ok();

    println!("\n{}", "─".repeat(60));
    println!();
    
    // Test 3: Full installer (disabled - needs no ALPM lock)    // Note: Run TEST 3 after a delay to ensure ALPM cache is fresh
    std::thread::sleep(std::time::Duration::from_millis(100));    test_case_3_package_installer().await.ok();
    
    println!("📋 TEST 3: Full Package Installer (install nano)");
    println!("{}", "─".repeat(60));

    Ok(())
}

async fn test_case_1_example() -> anyhow::Result<()> {
    let mut pool = AlpmPool::new();
    setup_example_repo(&mut pool);

    println!("📋 TEST 1: Example Repository (Firefox + Git)");
    println!("{}", "─".repeat(60));

    let pm = PackageManager::new(pool);
    
    match pm.plan_install(vec!["firefox", "git"]).await {
        Ok(result) => {
            PackageManager::display_install_plan(&result);
            println!("\n✅ Installation plan created successfully!");
        }
        Err(e) => {
            println!("\n❌ Error: {}", e);
        }
    }

    Ok(())
}

async fn test_case_2_real_repo() -> anyhow::Result<()> {
    println!("📋 TEST 2: Real ALPM Repository + Groups\n");

    match AlpmRepository::new() {
        Ok(repo) => {
            let stats = repo.get_stats();
            println!("{}\n", stats);

            match repo.load_sync_to_pool() {
                Ok(pool) => {
                    let pm = PackageManager::new(pool);
                    println!("✅ Successfully loaded real ALPM repository");
                    
                    // Test 2a: Single package
                    println!("\n🔹 Installing single package: nano");
                    match pm.plan_install(vec!["nano"]).await {
                        Ok(result) => {
                            PackageManager::display_install_plan(&result);
                        }
                        Err(e) => {
                            println!("⚠️  Could not plan install for nano: {}", e);
                        }
                    }

                    // Test 2b: Try to expand a group
                    println!("\n🔹 Attempting to expand group: base (if available)");
                    match repo.expand_names(vec!["base"]) {
                        Ok(expanded) => {
                            println!("✅ Group 'base' expanded to {} packages", expanded.len());
                            if expanded.len() <= 20 {
                                for pkg in &expanded {
                                    println!("   - {}", pkg);
                                }
                            } else {
                                for pkg in expanded.iter().take(10) {
                                    println!("   - {}", pkg);
                                }
                                println!("   ... and {} more", expanded.len() - 10);
                            }
                        }
                        Err(e) => {
                            println!("⚠️  'base' is not a group: {}", e);
                        }
                    }
                }
                Err(e) => {
                    println!("⚠️  Could not load repository: {}", e);
                }
            }
        }
        Err(e) => {
            println!("⚠️  ALPM not available: {}", e);
            println!("   (This is expected in non-Arch environments)");
        }
    }

    Ok(())
}

async fn test_case_3_package_installer() -> anyhow::Result<()> {
    use std::fs;
    use std::path::Path;

    println!("📋 TEST 3: Full Package Installer (install nano)");
    println!("{}", "─".repeat(60));

    let dest = "/tmp/pacman-ostree-test";
    //let _ = fs::remove_dir_all(dest);

    println!("📦 Testing full installation process...\n");

    match package_installer::install_packages(
        vec!["base"],
        dest,
        None,
    ).await {
        Ok(_) => {
            println!("\n✅ Installation completed successfully!\n");

            let structures = vec![
                ("/usr/share/pacman", false),
                ("/var/lib/pacman/local", false),
                ("/var/cache/pacman/pkg", false),
                ("/bin", false),
            ];

            println!("📁 Directory structure:");
            for (path, _is_file) in &structures {
                let full_path = format!("{}{}", dest, path);
                let exists = Path::new(&full_path).is_dir();
                println!("   {} {}", if exists { "✅" } else { "❌" }, path);
            }

            // Check database
            println!("\n📚 Package database:");
            let local_db = format!("{}/var/lib/pacman/local", dest);
            if Path::new(&local_db).exists() {
                match fs::read_dir(&local_db) {
                    Ok(entries) => {
                        let count = entries.count();
                        println!("   Found {} entries in local database", count);
                        
                        for entry in fs::read_dir(&local_db).unwrap_or_else(|_| panic!("Can't read")) {
                            if let Ok(entry) = entry {
                                let path = entry.path();
                                if path.is_dir() {
                                    let name = path.file_name().unwrap().to_string_lossy();
                                    let has_desc = path.join("desc").exists();
                                    let has_files = path.join("files").exists();
                                    println!("   📦 {} ({} desc, {} files)",
                                        name,
                                        if has_desc { "✅" } else { "❌" },
                                        if has_files { "✅" } else { "❌" }
                                    );
                                }
                            }
                        }
                    }
                    Err(_) => println!("   ⚠️  Could not read local database"),
                }
            }

        }
        Err(e) => {
            println!("⚠️  Installation failed: {}", e);
            let _ = fs::remove_dir_all(dest);
        }
    }

    Ok(())
}

fn setup_example_repo(pool: &mut AlpmPool) {
    // Firefox
    pool.add_package(AlpmPackage {
        name: "firefox".into(), version: "121.0".into(), pkgrel: "1".into(), repo: "extra".into(),
        size: 250_000_000,
        deps: vec![
            AlpmDep { name: "nss".into(), constraint: ">= 3.90".into() },
            AlpmDep { name: "gtk3".into(), constraint: ">= 3.22".into() },
        ],
        provides: vec![], conflicts: vec![],
    });

    // NSS
    pool.add_package(AlpmPackage {
        name: "nss".into(), version: "3.95".into(), pkgrel: "1".into(), repo: "extra".into(),
        size: 1_000_000,
        deps: vec![AlpmDep { name: "nspr".into(), constraint: ">= 4.35".into() }],
        provides: vec![], conflicts: vec![],
    });

    pool.add_package(AlpmPackage {
        name: "nspr".into(), version: "4.35".into(), pkgrel: "1".into(), repo: "core".into(),
        size: 500_000,
        deps: vec![], provides: vec![], conflicts: vec![],
    });

    // GTK3
    pool.add_package(AlpmPackage {
        name: "gtk3".into(), version: "3.24.39".into(), pkgrel: "2".into(), repo: "extra".into(),
        size: 5_000_000,
        deps: vec![
            AlpmDep { name: "glib2".into(), constraint: ">= 2.66.0".into() },
            AlpmDep { name: "cairo".into(), constraint: ">= 1.14.0".into() },
        ],
        provides: vec![], conflicts: vec![],
    });

    pool.add_package(AlpmPackage {
        name: "glib2".into(), version: "2.78.3".into(), pkgrel: "1".into(), repo: "core".into(),
        size: 5_000_000,
        deps: vec![], provides: vec![], conflicts: vec![],
    });

    pool.add_package(AlpmPackage {
        name: "cairo".into(), version: "1.18.0".into(), pkgrel: "2".into(), repo: "extra".into(),
        size: 1_000_000,
        deps: vec![],
        provides: vec![], conflicts: vec![],
    });

    // Git
    pool.add_package(AlpmPackage {
        name: "git".into(), version: "2.43.0".into(), pkgrel: "1".into(), repo: "extra".into(),
        size: 10_000_000,
        deps: vec![
            AlpmDep { name: "curl".into(), constraint: ">= 0".into() },
            AlpmDep { name: "openssl".into(), constraint: ">= 3.0".into() },
        ],
        provides: vec![], conflicts: vec![],
    });

    pool.add_package(AlpmPackage {
        name: "curl".into(), version: "8.5.0".into(), pkgrel: "1".into(), repo: "core".into(),
        size: 2_000_000,
        deps: vec![AlpmDep { name: "openssl".into(), constraint: ">= 3.0".into() }],
        provides: vec![], conflicts: vec![],
    });

    pool.add_package(AlpmPackage {
        name: "openssl".into(), version: "3.2.0".into(), pkgrel: "1".into(), repo: "core".into(),
        size: 5_000_000,
        deps: vec![], provides: vec![], conflicts: vec![],
    });
}