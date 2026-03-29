//! Zaawansowany resolver pakietów dla pacman-ostree
//!
//! Ten moduł provides integracji libsolv z ALPM (Arch Linux Package Manager)
//! w celu rozwiązywania zależności pakietów i zarządzania konfliktami.
//!
//! # Przykłady
//!
//! ```rust,no_run
//! use pacman_ostree::solver::{SolvResolver, SolverError};
//!
//! // Stwórz resolver
//! let resolver = SolvResolver::new()?;
//!
//! // Rozwiąż instalację pakietów
//! let result = resolver.resolve_install(&vec!["firefox".to_string(), "vim".to_string()])?;
//! println!("Do instalacji: {:?}", result.to_install);
//!
//! // Pobierz informacje o pakiecie
//! let info = resolver.get_package_info("linux")?;
//! println!("Pakiet: {}, wersja: {}", info.name, info.version);
//!
//! // Optymalizuj system
//! let unused = resolver.optimize()?;
//! println!("Nieużywane pakiety: {:?}", unused.to_remove);
//! # Ok::<(), SolverError>(())
//! ```

use std::error::Error;
use std::ffi::{CStr, CString};
use std::ptr;
use std::fmt;

// ─────────────────────────────────────────────────────────────
// libsolv bindings (uproszczone - w praktyce użyj bindgen)
// ─────────────────────────────────────────────────────────────

#[allow(non_camel_case_types)]
mod solv_sys {
    use std::os::raw::{c_char, c_int, c_void};

    pub const SOLVER_INSTALL: c_int = 0x0100;
    pub const SOLVER_ERASE: c_int = 0x0200;
    pub const SOLVER_UPDATE: c_int = 0x0300;
    
    pub const SOLVER_FLAG_ALLOW_UNINSTALL: c_int = 1;
    pub const SOLVER_FLAG_ALLOW_DOWNGRADE: c_int = 2;
    
    pub const REPO_REUSE_REPODATA: c_int = 1;
    pub const REPO_NO_INTERNALIZE: c_int = 2;

    #[repr(C)]
    pub struct Pool { _opaque: [u8; 0] }
    
    #[repr(C)]
    pub struct Repo { _opaque: [u8; 0] }
    
    #[repr(C)]
    pub struct Solver { _opaque: [u8; 0] }
    
    #[repr(C)]
    pub struct Transaction { _opaque: [u8; 0] }
    
    #[repr(C)]
    pub struct Queue {
        pub elements: *mut c_int,
        pub count: c_int,
        pub alloc: *mut c_void,
        pub left: c_int,
    }
    
    pub type Id = c_int;

    extern "C" {
        // Pool
        pub fn pool_create() -> *mut Pool;
        pub fn pool_free(pool: *mut Pool);
        pub fn pool_setarch(pool: *mut Pool, arch: *const c_char);
        pub fn pool_addfileprovides(pool: *mut Pool);
        pub fn pool_createwhatprovides(pool: *mut Pool);
        pub fn pool_str2id(pool: *mut Pool, s: *const c_char, create: c_int) -> Id;
        pub fn pool_id2str(pool: *mut Pool, id: Id) -> *const c_char;
        pub fn pool_id2solvable(pool: *mut Pool, id: Id) -> *mut Solvable;
        
        // Repo
        pub fn repo_create(pool: *mut Pool, name: *const c_char) -> *mut Repo;
        pub fn repo_free(repo: *mut Repo, reuseids: c_int);
        pub fn repo_add_solv(repo: *mut Repo, fp: *mut libc::FILE, flags: c_int) -> c_int;
        pub fn repo_internalize(repo: *mut Repo);
        pub fn repo_add_deparray(
            repo: *mut Repo,
            solvable_id: Id,
            keyname: Id,
            dependencies: *const c_char,
            flags: c_int,
        ) -> c_int;
        
        // Solvable
        pub fn repo_add_solvable(repo: *mut Repo) -> Id;
        pub fn solvable_lookup_str(s: *mut Solvable, keyname: Id) -> *const c_char;
        
        // Solver
        pub fn solver_create(pool: *mut Pool) -> *mut Solver;
        pub fn solver_free(solver: *mut Solver);
        pub fn solver_set_flag(solver: *mut Solver, flag: c_int, value: c_int) -> c_int;
        pub fn solver_solve(solver: *mut Solver, job: *mut Queue) -> c_int;
        pub fn solver_problem_count(solver: *mut Solver) -> c_int;
        pub fn solver_get_transaction(solver: *mut Solver) -> *mut Transaction;
        
        // Transaction  
        pub fn transaction_order(trans: *mut Transaction, flags: c_int);
        pub fn transaction_installedresult(trans: *mut Transaction, queue: *mut Queue);
        pub fn transaction_free(trans: *mut Transaction);
        
        // Queue
        pub fn queue_init(q: *mut Queue);
        pub fn queue_free(q: *mut Queue);
        pub fn queue_push2(q: *mut Queue, id1: Id, id2: Id);
        
        // Selection (high-level API)
        pub fn selection_make(
            pool: *mut Pool,
            sel: *mut Queue,
            name: *const c_char,
            flags: c_int,
        ) -> c_int;
    }
    
    #[repr(C)]
    pub struct Solvable {
        pub repo: *mut Repo,
        pub name: Id,
        pub arch: Id,
        pub evr: Id,
        pub vendor: Id,
        pub provides: Id,
        pub obsoletes: Id,
        pub conflicts: Id,
        pub requires: Id,
        pub recommends: Id,
        pub suggests: Id,
        pub supplements: Id,
        pub enhances: Id,
    }

    // Stałe dla keynames
    pub const SOLVABLE_NAME: Id = 1;
    pub const SOLVABLE_EVR: Id = 3;
    pub const SOLVABLE_ARCH: Id = 4;
}

use solv_sys::*;

// ─────────────────────────────────────────────────────────────
// Custom Error Types
// ─────────────────────────────────────────────────────────────

/// Typy błędów dla operacji resolvera
#[derive(Debug)]
pub enum SolverError {
    /// Błąd z libsolv
    LibsolvFailed(String),
    /// Pakiet nie znaleziony
    PackageNotFound(String),
    /// Konflikt zależności
    DependencyConflict(Vec<String>),
    /// Błąd z ALPM
    AlpmError(String),
    /// Błąd FFI
    FfiError(String),
}

impl fmt::Display for SolverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SolverError::LibsolvFailed(msg) => write!(f, "libsolv failed: {}", msg),
            SolverError::PackageNotFound(pkg) => write!(f, "Package not found: {}", pkg),
            SolverError::DependencyConflict(probs) => {
                write!(f, "Dependency conflicts: {}", probs.join("; "))
            }
            SolverError::AlpmError(msg) => write!(f, "ALPM error: {}", msg),
            SolverError::FfiError(msg) => write!(f, "FFI error: {}", msg),
        }
    }
}

impl Error for SolverError {}

// ─────────────────────────────────────────────────────────────
// Safe Rust wrapper dla libsolv
// ─────────────────────────────────────────────────────────────

/// Wewnętrzna struktura do zarządzania poolem libsolv
/// 
/// Przechowuje stany pozdsyst do bilancowania potrzeb libsolv i ALPM
struct SolvPool {
    pool: *mut Pool,
    installed_repo: Option<*mut Repo>,
    available_repo: Option<*mut Repo>,
}

impl SolvPool {
    pub fn new() -> Result<Self, SolverError> {
        let pool = unsafe { pool_create() };
        if pool.is_null() {
            return Err(SolverError::LibsolvFailed("Failed to create pool".to_string()));
        }
        
        // Ustaw architekturę (x86_64 dla Arch)
        let arch = CString::new("x86_64")
            .map_err(|e| SolverError::FfiError(e.to_string()))?;
        unsafe { pool_setarch(pool, arch.as_ptr()) };
        
        Ok(SolvPool {
            pool,
            installed_repo: None,
            available_repo: None,
        })
    }
    
    /// Ładuje zainstalowane pakiety z localdb do "installed" repo
    pub fn load_installed(&mut self) -> Result<(), SolverError> {
        let name = CString::new("@System")
            .map_err(|e| SolverError::FfiError(e.to_string()))?;
        let repo = unsafe { repo_create(self.pool, name.as_ptr()) };
        
        // Tutaj załaduj pakiety z /var/lib/pacman/local
        self.load_pacman_localdb(repo)?;
        
        unsafe { repo_internalize(repo) };
        self.installed_repo = Some(repo);
        
        Ok(())
    }
    
    /// Ładuje dostępne pakiety z syncdbs
    pub fn load_available(&mut self) -> Result<(), SolverError> {
        let name = CString::new("available")
            .map_err(|e| SolverError::FfiError(e.to_string()))?;
        let repo = unsafe { repo_create(self.pool, name.as_ptr()) };
        
        // Załaduj pakiety z pacman syncdbs
        self.load_pacman_syncdbs(repo)?;
        
        unsafe { repo_internalize(repo) };
        self.available_repo = Some(repo);
        
        Ok(())
    }
    
    /// Finalizuje pool - tworzy indeksy provides
    pub fn prepare(&mut self) {
        unsafe {
            pool_addfileprovides(self.pool);
            pool_createwhatprovides(self.pool);
        }
    }
    
    fn load_pacman_localdb(&self, repo: *mut Repo) -> Result<(), SolverError> {
        use alpm_utils::alpm_with_conf;
        
        let conf = pacmanconf::Config::new()
            .map_err(|e| SolverError::AlpmError(e.to_string()))?;
        let alpm = alpm_with_conf(&conf)
            .map_err(|e| SolverError::AlpmError(e.to_string()))?;
        
        for pkg in alpm.localdb().pkgs().iter() {
            self.add_solvable_from_alpm_pkg(repo, &pkg)?;
        }
        
        Ok(())
    }
    
    fn load_pacman_syncdbs(&self, repo: *mut Repo) -> Result<(), SolverError> {
        use alpm_utils::alpm_with_conf;
        
        let conf = pacmanconf::Config::new()
            .map_err(|e| SolverError::AlpmError(e.to_string()))?;
        let alpm = alpm_with_conf(&conf)
            .map_err(|e| SolverError::AlpmError(e.to_string()))?;
        
        for db in alpm.syncdbs().iter() {
            for pkg in db.pkgs().iter() {
                self.add_solvable_from_alpm_pkg(repo, &pkg)?;
            }
        }
        
        Ok(())
    }
    
    fn add_solvable_from_alpm_pkg(
        &self,
        repo: *mut Repo,
        pkg: &alpm::Package,
    ) -> Result<(), SolverError> {
        let id = unsafe { repo_add_solvable(repo) };
        let solvable = unsafe { pool_id2solvable(self.pool, id) };
        
        if solvable.is_null() {
            return Err(SolverError::LibsolvFailed("Failed to create solvable".to_string()));
        }
        
        // Ustaw podstawowe pola
        let name = CString::new(pkg.name())
            .map_err(|e| SolverError::FfiError(e.to_string()))?;
        let version = CString::new(pkg.version())
            .map_err(|e| SolverError::FfiError(e.to_string()))?;
        let arch = CString::new(pkg.arch().unwrap_or("any"))
            .map_err(|e| SolverError::FfiError(e.to_string()))?;
        
        // Przygotuj string zależności
        let deps_str = pkg.depends().iter()
            .map(|d| d.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let deps_cstr = if !deps_str.is_empty() {
            Some(CString::new(deps_str)
                .map_err(|e| SolverError::FfiError(e.to_string()))?)
        } else {
            None
        };
        
        // Przygotuj string konfliktów
        let conflicts_str = pkg.conflicts().iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let conflicts_cstr = if !conflicts_str.is_empty() {
            Some(CString::new(conflicts_str)
                .map_err(|e| SolverError::FfiError(e.to_string()))?)
        } else {
            None
        };
        
        // Przygotuj string zamienników (replaces)
        let replaces_str = pkg.replaces().iter()
            .map(|r| r.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let replaces_cstr = if !replaces_str.is_empty() {
            Some(CString::new(replaces_str)
                .map_err(|e| SolverError::FfiError(e.to_string()))?)
        } else {
            None
        };
        
        unsafe {
            (*solvable).name = pool_str2id(self.pool, name.as_ptr(), 1);
            (*solvable).evr = pool_str2id(self.pool, version.as_ptr(), 1);
            (*solvable).arch = pool_str2id(self.pool, arch.as_ptr(), 1);
            
            // Ustawdź zależności
            if let Some(deps_cstr) = deps_cstr {
                (*solvable).requires = pool_str2id(self.pool, deps_cstr.as_ptr(), 1);
            }
            
            // Ustaw konflikty
            if let Some(conflicts_cstr) = conflicts_cstr {
                (*solvable).conflicts = pool_str2id(self.pool, conflicts_cstr.as_ptr(), 1);
            }
            
            // Ustaw zamienniki (replaces -> obsoletes)
            if let Some(replaces_cstr) = replaces_cstr {
                (*solvable).obsoletes = pool_str2id(self.pool, replaces_cstr.as_ptr(), 1);
            }
            
            // Uwaga: provides można dodać jeśli pakiet ma explicit provides w PKGBUILD
            // Dla większości pakietów provides = name, więc nie jest konieczne
        }
        
        Ok(())
    }
}

impl Drop for SolvPool {
    fn drop(&mut self) {
        if let Some(repo) = self.installed_repo {
            unsafe { repo_free(repo, 0) };
        }
        if let Some(repo) = self.available_repo {
            unsafe { repo_free(repo, 0) };
        }
        unsafe { pool_free(self.pool) };
    }
}

// ─────────────────────────────────────────────────────────────
// Główny resolver używający libsolv
// ─────────────────────────────────────────────────────────────

/// Główny resolver pakietów
/// 
/// Integruje libsolv z ALPM do rozwiązywania zależności,
/// zarządzania konfliktami i optymalizacji systemu pakietów.
pub struct SolvResolver {
    pool: SolvPool,
}

/// Wynik resolwacji instalacji/usunięcia pakietów
#[derive(Debug, Clone)]
pub struct ResolveResult {
    /// Pakiety do instalacji
    pub to_install: Vec<String>,
    /// Pakiety do usunięcia
    pub to_remove: Vec<String>,
    /// Problemy natrafione podczas resolwacji
    pub problems: Vec<String>,
}

/// Informacja o reverse dependencies pakietu
#[derive(Debug)]
pub struct ReverseDeps {
    /// Nazwa pakietu
    pub package: String,
    /// Lista pakietów, które zależą od danego pakietu
    pub dependents: Vec<String>,
}

impl SolvResolver {
    pub fn new() -> Result<Self, SolverError> {
        let mut pool = SolvPool::new()?;
        pool.load_installed()?;
        pool.load_available()?;
        pool.prepare();
        
        Ok(SolvResolver { pool })
    }
    
    /// Rozwiązuje instalację pakietów - zwraca pełną listę do instalacji
    pub fn resolve_install(&self, packages: &[String]) -> Result<ResolveResult, SolverError> {
        let solver = unsafe { solver_create(self.pool.pool) };
        if solver.is_null() {
            return Err(SolverError::LibsolvFailed("Failed to create solver".to_string()));
        }
        
        // Zezwól na rozwiązywanie konfliktów
        unsafe {
            solver_set_flag(solver, SOLVER_FLAG_ALLOW_DOWNGRADE, 1);
        }
        
        // Przygotuj job queue
        let mut job = Queue {
            elements: ptr::null_mut(),
            count: 0,
            alloc: ptr::null_mut(),
            left: 0,
        };
        unsafe { queue_init(&mut job) };
        
        // Dodaj pakiety do instalacji
        for pkg_name in packages {
            let name = CString::new(pkg_name.as_str())
                .map_err(|e| SolverError::FfiError(e.to_string()))?;
            let id = unsafe { pool_str2id(self.pool.pool, name.as_ptr(), 0) };
            
            if id == 0 {
                unsafe { 
                    queue_free(&mut job);
                    solver_free(solver);
                }
                return Err(SolverError::PackageNotFound(pkg_name.clone()));
            }
            
            // SOLVER_INSTALL | SOLVER_SOLVABLE_NAME
            unsafe { queue_push2(&mut job, SOLVER_INSTALL | 0x1000, id) };
        }
        
        // Rozwiąż
        let problems = unsafe { solver_solve(solver, &mut job) };
        
        let result = if problems > 0 {
            // Zbierz problemy
            let problem_count = unsafe { solver_problem_count(solver) };
            let mut problem_msgs = Vec::new();
            
            for i in 1..=problem_count {
                problem_msgs.push(format!("Problem {}: dependency conflict", i));
            }
            
            ResolveResult {
                to_install: vec![],
                to_remove: vec![],
                problems: problem_msgs,
            }
        } else {
            // Pobierz transakcję
            let trans = unsafe { solver_get_transaction(solver) };
            let mut install_queue = Queue {
                elements: ptr::null_mut(),
                count: 0,
                alloc: ptr::null_mut(),
                left: 0,
            };
            unsafe { 
                queue_init(&mut install_queue);
                transaction_installedresult(trans, &mut install_queue);
            }
            
            let mut to_install = Vec::new();
            for i in 0..install_queue.count {
                let id = unsafe { *install_queue.elements.offset(i as isize) };
                let solvable = unsafe { pool_id2solvable(self.pool.pool, id) };
                if !solvable.is_null() {
                    let name_id = unsafe { (*solvable).name };
                    let name_ptr = unsafe { pool_id2str(self.pool.pool, name_id) };
                    if !name_ptr.is_null() {
                        let name = unsafe { CStr::from_ptr(name_ptr) };
                        to_install.push(name.to_string_lossy().into_owned());
                    }
                }
            }
            
            unsafe {
                queue_free(&mut install_queue);
                transaction_free(trans);
            }
            
            ResolveResult {
                to_install,
                to_remove: vec![],
                problems: vec![],
            }
        };
        
        unsafe {
            queue_free(&mut job);
            solver_free(solver);
        }
        
        Ok(result)
    }
    
    /// Sprawdza reverse dependencies - co zależy od danego pakietu
    pub fn check_reverse_deps(&self, package: &str) -> Result<ReverseDeps, SolverError> {
        use alpm_utils::alpm_with_conf;
        
        let conf = pacmanconf::Config::new()
            .map_err(|e| SolverError::AlpmError(e.to_string()))?;
        let alpm = alpm_with_conf(&conf)
            .map_err(|e| SolverError::AlpmError(e.to_string()))?;
        
        let pkg = alpm.localdb().pkg(package)
            .map_err(|_| SolverError::PackageNotFound(package.to_string()))?;
        
        let dependents: Vec<String> = pkg
            .required_by()
            .iter()
            .map(|s| s.to_string())
            .collect();
        
        Ok(ReverseDeps {
            package: package.to_string(),
            dependents,
        })
    }
    
    /// Rozwiązuje usunięcie z uwzględnieniem reverse deps
    pub fn resolve_remove(
        &self, 
        packages: &[String],
        cascade: bool,  // czy usunąć też zależne pakiety
    ) -> Result<ResolveResult, SolverError> {
        let mut to_remove = packages.to_vec();
        let mut problems = Vec::new();
        
        for pkg in packages {
            let rev_deps = self.check_reverse_deps(pkg)?;
            
            if !rev_deps.dependents.is_empty() {
                if cascade {
                    // Dodaj zależne pakiety do usunięcia
                    for dep in &rev_deps.dependents {
                        if !to_remove.contains(dep) {
                            to_remove.push(dep.clone());
                        }
                    }
                } else {
                    // Zgłoś problem
                    problems.push(format!(
                        "Cannot remove '{}': required by {}",
                        pkg,
                        rev_deps.dependents.join(", ")
                    ));
                }
            }
        }
        
        if !problems.is_empty() && !cascade {
            return Ok(ResolveResult {
                to_install: vec![],
                to_remove: vec![],
                problems,
            });
        }
        
        Ok(ResolveResult {
            to_install: vec![],
            to_remove,
            problems: vec![],
        })
    }
}

impl Default for SolvResolver {
    fn default() -> Self {
        Self::new().expect("Failed to create default SolvResolver")
    }
}

// ─────────────────────────────────────────────────────────────
// Zusätzliche útilný metody
// ─────────────────────────────────────────────────────────────

impl SolvResolver {
    /// Pobiera listę wszystkich zainstalowanych pakietów
    pub fn get_installed_packages(&self) -> Result<Vec<String>, SolverError> {
        use alpm_utils::alpm_with_conf;
        
        let conf = pacmanconf::Config::new()
            .map_err(|e| SolverError::AlpmError(e.to_string()))?;
        let alpm = alpm_with_conf(&conf)
            .map_err(|e| SolverError::AlpmError(e.to_string()))?;
        
        let packages: Vec<String> = alpm
            .localdb()
            .pkgs()
            .iter()
            .map(|pkg| pkg.name().to_string())
            .collect();
        
        Ok(packages)
    }
    
    /// Sprawdza czy pakiet jest zainstalowany
    pub fn is_installed(&self, package: &str) -> Result<bool, SolverError> {
        use alpm_utils::alpm_with_conf;
        
        let conf = pacmanconf::Config::new()
            .map_err(|e| SolverError::AlpmError(e.to_string()))?;
        let alpm = alpm_with_conf(&conf)
            .map_err(|e| SolverError::AlpmError(e.to_string()))?;
        
        Ok(alpm.localdb().pkg(package).is_ok())
    }
    
    /// Pobiera informacje o pakiecie
    pub fn get_package_info(&self, package: &str) -> Result<PackageInfo, SolverError> {
        use alpm_utils::alpm_with_conf;
        
        let conf = pacmanconf::Config::new()
            .map_err(|e| SolverError::AlpmError(e.to_string()))?;
        let alpm = alpm_with_conf(&conf)
            .map_err(|e| SolverError::AlpmError(e.to_string()))?;
        
        let pkg = alpm.localdb().pkg(package)
            .map_err(|_| SolverError::PackageNotFound(package.to_string()))?;
        
        Ok(PackageInfo {
            name: pkg.name().to_string(),
            version: pkg.version().to_string(),
            arch: pkg.arch().unwrap_or("any").to_string(),
            description: pkg.desc().unwrap_or("").to_string(),
            installed_size: pkg.isize(),
            dependencies: pkg.depends().iter().map(|d| d.to_string()).collect(),
            required_by: pkg.required_by().iter().map(|r| r.to_string()).collect(),
        })
    }
    
    /// Optymalizuje obecny system - usuwa nieużywane pakiety
    pub fn optimize(&self) -> Result<ResolveResult, SolverError> {
        use alpm_utils::alpm_with_conf;
        
        let conf = pacmanconf::Config::new()
            .map_err(|e| SolverError::AlpmError(e.to_string()))?;
        let alpm = alpm_with_conf(&conf)
            .map_err(|e| SolverError::AlpmError(e.to_string()))?;
        
        let mut unused = Vec::new();
        
        // Iteruj po zainstalowanych pakietach
        for pkg in alpm.localdb().pkgs().iter() {
            let required_by = pkg.required_by();
            let pkg_reason = pkg.reason();
            
            // Potrzebna jest implementacja matchowania z alpm::PackageReason
            // Dla teraz używamy prostego sprawdzenia
            let is_explicit = pkg.reason() != alpm::PackageReason::Depend;
            
            // Jeśli pakiet jest zależnością (reason=Depend) i nic go nie wymaga, 
            // jest kandydatem do usunięcia
            if !is_explicit && required_by.is_empty() {
                unused.push(pkg.name().to_string());
            }
        }
        
        Ok(ResolveResult {
            to_install: vec![],
            to_remove: unused,
            problems: vec![],
        })
    }
}

// ─────────────────────────────────────────────────────────────
// PackageInfo struktura dla szczegółów pakietu
// ─────────────────────────────────────────────────────────────

/// Szczegółowe informacje o pakiecie
#[derive(Debug, Clone)]
pub struct PackageInfo {
    /// Nazwa pakietu
    pub name: String,
    /// Wersja pakietu
    pub version: String,
    /// Architektura (x86_64, i686, any, itp.)
    pub arch: String,
    /// Opis pakietu
    pub description: String,
    /// Rozmiar zainstalowany w bajtach
    pub installed_size: usize,
    /// Lista zależności tego pakietu
    pub dependencies: Vec<String>,
    /// Lista pakietów, które zależy od tego pakietu
    pub required_by: Vec<String>,
}
