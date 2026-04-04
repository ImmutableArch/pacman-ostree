mod package_manager;
mod package_solver;

use package_solver::{AlpmPool, AlpmPackage, AlpmDep, AlpmProvide};
use package_manager::{AlpmRepository, PackageManager};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("╔════════════════════════════════════════════════════════╗");
    println!("║       ALPM Dependency Resolver + Package Manager       ║");
    println!("╚════════════════════════════════════════════════════════╝\n");

    // Test 1: Usar hardcoded example repo (dla testów bez access do pacmana)
    test_case_1_example().await?;
    
    println!("\n{}", "─".repeat(60));
    println!();
    
    // Test 2: Real ALPM repository (jeśli dostępny)
    test_case_2_real_repo().await.ok();

    Ok(())
}

async fn test_case_1_example() -> anyhow::Result<()> {
    let mut pool = AlpmPool::new();
    setup_example_repo(&mut pool);

    println!("📋 TEST 1: Example Repository (Firefox + Git)");
    println!("{}", "─".repeat(60));

    let pm = PackageManager::new(pool);
    
    // Zaplanuj instalację firefox'a i gita
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
    println!("📋 TEST 2: Real ALPM Repository\n");

    match AlpmRepository::new() {
        Ok(repo) => {
            let stats: String = repo.get_stats();
            println!("{}\n", stats);

            // Spróbuj załadować rzeczywiste pakiety
            match repo.load_sync_to_pool() {
                Ok(pool) => {
                    let pm = PackageManager::new(pool);
                    println!("✅ Successfully loaded real ALPM repository");
                    
                    // Spróbuj zaplanować instalację nano
                    match pm.plan_install(vec!["nano"]).await {
                        Ok(result) => {
                            PackageManager::display_install_plan(&result);
                        }
                        Err(e) => {
                            println!("⚠️  Could not plan install for nano: {}", e);
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

fn setup_example_repo(pool: &mut AlpmPool) {
    // ── firefox ───────────────────────────────
    pool.add_package(AlpmPackage {
        name: "firefox".into(), version: "121.0".into(), pkgrel: "1".into(), repo: "extra".into(),
        size: 250_000_000, // 250MB
        deps: vec![
            AlpmDep { name: "nss".into(),      constraint: ">= 3.90".into() },
            AlpmDep { name: "gtk3".into(),     constraint: ">= 3.22".into() },
            AlpmDep { name: "libpulse".into(), constraint: ">= 0".into() },
        ],
        provides: vec![], conflicts: vec!["firefox-esr".into()],
    });

    // ── nss (dwie wersje) ─────────────────────
    pool.add_package(AlpmPackage {
        name: "nss".into(), version: "3.95".into(), pkgrel: "1".into(), repo: "extra".into(),
        size: 1_000_000, // 1MB
        deps: vec![AlpmDep { name: "nspr".into(), constraint: ">= 4.35".into() }],
        provides: vec![], conflicts: vec![],
    });
    pool.add_package(AlpmPackage {
        name: "nss".into(), version: "3.89".into(), pkgrel: "1".into(), repo: "extra".into(),
        size: 1_000_000, // 1MB
        deps: vec![AlpmDep { name: "nspr".into(), constraint: ">= 4.34".into() }],
        provides: vec![], conflicts: vec![],
    });
    pool.add_package(AlpmPackage {
        name: "nspr".into(), version: "4.35".into(), pkgrel: "1".into(), repo: "core".into(),
        size: 500_000, // 500KB
        deps: vec![], provides: vec![], conflicts: vec![],
    });

    // ── gtk3 ──────────────────────────────────
    pool.add_package(AlpmPackage {
        name: "gtk3".into(), version: "3.24.39".into(), pkgrel: "2".into(), repo: "extra".into(),
        size: 5_000_000, // 5MB
        deps: vec![
            AlpmDep { name: "glib2".into(), constraint: ">= 2.66.0".into() },
            AlpmDep { name: "cairo".into(), constraint: ">= 1.14.0".into() },
            AlpmDep { name: "pango".into(), constraint: ">= 1.44.0".into() },
        ],
        provides: vec![], conflicts: vec![],
    });
    pool.add_package(AlpmPackage {
        name: "glib2".into(), version: "2.78.3".into(), pkgrel: "1".into(), repo: "core".into(),
        size: 5_000_000, // 5MB
        deps: vec![], provides: vec![], conflicts: vec![],
    });
    pool.add_package(AlpmPackage {
        name: "cairo".into(), version: "1.18.0".into(), pkgrel: "2".into(), repo: "extra".into(),
        size: 1_000_000, // 1MB
        deps: vec![AlpmDep { name: "glib2".into(), constraint: ">= 2.0".into() }],
        provides: vec![], conflicts: vec![],
    });
    pool.add_package(AlpmPackage {
        name: "pango".into(), version: "1.51.0".into(), pkgrel: "1".into(), repo: "extra".into(),
        size: 1_000_000, // 1MB
        deps: vec![
            AlpmDep { name: "cairo".into(), constraint: ">= 1.12.10".into() },
            AlpmDep { name: "glib2".into(), constraint: ">= 2.62".into() },
        ],
        provides: vec![], conflicts: vec![],
    });

    // ── libpulse: wirtualny, dostarczany przez pulseaudio lub pipewire-pulse ──
    pool.add_package(AlpmPackage {
        name: "pulseaudio".into(), version: "17.0".into(), pkgrel: "2".into(), repo: "extra".into(),
        size: 10_000_000, // 10MB
        deps: vec![],
        provides: vec![AlpmProvide { virtual_name: "libpulse".into(), virtual_version: "17.0".into() }],
        conflicts: vec!["pipewire-pulse".into()],
    });
    pool.add_package(AlpmPackage {
        name: "pipewire-pulse".into(), version: "1.0.1".into(), pkgrel: "1".into(), repo: "extra".into(),
        size: 5_000_000, // 5MB
        deps: vec![AlpmDep { name: "pipewire".into(), constraint: ">= 1.0.1".into() }],
        provides: vec![AlpmProvide { virtual_name: "libpulse".into(), virtual_version: "17.0".into() }],
        conflicts: vec!["pulseaudio".into()],
    });
    pool.add_package(AlpmPackage {
        name: "pipewire".into(), version: "1.0.1".into(), pkgrel: "1".into(), repo: "extra".into(),
        size: 5_000_000, // 5MB
        deps: vec![], provides: vec![], conflicts: vec![],
    });

    // ── git ───────────────────────────────────
    pool.add_package(AlpmPackage {
        name: "git".into(), version: "2.43.0".into(), pkgrel: "1".into(), repo: "extra".into(),
        size: 10_000_000, // 10MB
        deps: vec![
            AlpmDep { name: "curl".into(),  constraint: ">= 0".into() },
            AlpmDep { name: "expat".into(), constraint: ">= 0".into() },
            AlpmDep { name: "perl".into(),  constraint: ">= 5.14.0".into() },
            AlpmDep { name: "pcre2".into(), constraint: ">= 0".into() },
        ],
        provides: vec![], conflicts: vec![],
    });
    pool.add_package(AlpmPackage {
        name: "curl".into(), version: "8.5.0".into(), pkgrel: "1".into(), repo: "core".into(),
        size: 2_000_000, // 2MB
        deps: vec![
            AlpmDep { name: "openssl".into(), constraint: ">= 3.0".into() },
            AlpmDep { name: "zlib".into(),    constraint: ">= 1.2.3".into() },
        ],
        provides: vec![], conflicts: vec![],
    });
    pool.add_package(AlpmPackage {
        name: "openssl".into(), version: "3.2.0".into(), pkgrel: "1".into(), repo: "core".into(),
        size: 5_000_000, // 5MB
        deps: vec![], provides: vec![], conflicts: vec![],
    });
    pool.add_package(AlpmPackage {
        name: "zlib".into(), version: "1.3.1".into(), pkgrel: "1".into(), repo: "core".into(),
        size: 1_000_000, // 1MB
        deps: vec![], provides: vec![], conflicts: vec![],
    });
    pool.add_package(AlpmPackage {
        name: "expat".into(), version: "2.5.0".into(), pkgrel: "1".into(), repo: "core".into(),
        size: 1_000_000, // 1MB
        deps: vec![], provides: vec![], conflicts: vec![],
    });
    pool.add_package(AlpmPackage {
        name: "perl".into(), version: "5.38.2".into(), pkgrel: "1".into(), repo: "core".into(),
        size: 30_000_000, // 30MB
        deps: vec![], provides: vec![], conflicts: vec![],
    });
    pool.add_package(AlpmPackage {
        name: "pcre2".into(), version: "10.42".into(), pkgrel: "1".into(), repo: "core".into(),
        size: 1_000_000, // 1MB
        deps: vec![], provides: vec![], conflicts: vec![],
    });
}