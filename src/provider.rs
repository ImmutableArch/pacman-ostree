use std::cell::RefCell;

use resolvo::{
    Candidates, Dependencies, DependencyProvider, HintDependenciesAvailable,
    Interner, KnownDependencies, NameId, Requirement, SolvableId,
    SolverCache, VersionSetId, VersionSetUnionId,
};

use crate::pool::AlpmPool;

/// Implementacja DependencyProvider + Interner dla ekosystemu ALPM
pub struct AlpmDependencyProvider {
    pool: RefCell<AlpmPool>,
}

impl AlpmDependencyProvider {
    pub fn new(pool: AlpmPool) -> Self {
        Self {
            pool: RefCell::new(pool),
        }
    }

    pub fn pool(&self) -> std::cell::Ref<'_, AlpmPool> {
        self.pool.borrow()
    }

    pub fn pool_mut(&self) -> std::cell::RefMut<'_, AlpmPool> {
        self.pool.borrow_mut()
    }
}

/// Interner pozwala resolvo na pytanie o wyświetlalne nazwy pakietów,
/// wersje i version sety.
impl Interner for AlpmDependencyProvider {
    fn display_solvable(&self, solvable: SolvableId) -> impl std::fmt::Display + '_ {
        let pool = self.pool.borrow();
        let s = pool.resolve_solvable(solvable);
        format!("{}-{}", s.name, s.version)
    }

    fn display_name(&self, name: NameId) -> impl std::fmt::Display + '_ {
        let pool = self.pool.borrow();
        pool.resolve_name(name)
            .unwrap_or("<unknown>")
            .to_string()
    }

    fn display_version_set(&self, version_set: VersionSetId) -> impl std::fmt::Display + '_ {
        let pool = self.pool.borrow();
        let name_id = pool.resolve_version_set_name(version_set);
        let constraint = pool.resolve_version_set_constraint(version_set);
        if let Some(nid) = name_id {
            let name = pool.resolve_name(nid).unwrap_or("?").to_string();
            format!("{} {}", name, constraint)
        } else {
            constraint
        }
    }

    fn display_string(&self, _string_id: resolvo::StringId) -> impl std::fmt::Display + '_ {
        "<string>"
    }

    fn version_set_name(&self, version_set: VersionSetId) -> NameId {
        self.pool
            .borrow()
            .resolve_version_set_name(version_set)
            .unwrap_or(NameId::from(0u32))
    }

    fn solvable_name(&self, solvable: SolvableId) -> NameId {
        let pool = self.pool.borrow();
        let s = pool.resolve_solvable(solvable);
        // Znajdź NameId dla nazwy solvable
        pool.package_names()
            .get(&s.name)
            .copied()
            .unwrap_or(NameId::from(0u32))
    }
}

/// DependencyProvider - serce integracji z resolvo
impl DependencyProvider for AlpmDependencyProvider {
    /// Filtruje kandydatów spełniających (lub niespełniających) dany version set
    async fn filter_candidates(
        &self,
        candidates: &[SolvableId],
        version_set: VersionSetId,
        inverse: bool,
    ) -> Vec<SolvableId> {
        self.pool
            .borrow()
            .filter_by_version_set(candidates, version_set, inverse)
    }

    /// Zwraca listę kandydatów dla danej nazwy pakietu
    async fn get_candidates(&self, name: NameId) -> Option<Candidates> {
        self.pool.borrow().get_candidates_for(name)
    }

    /// Sortuje kandydatów - resolvo zaczyna od najwyższej wersji
    async fn sort_candidates(
        &self,
        _solver: &SolverCache<Self>,
        solvables: &mut [SolvableId],
    ) {
        // Sortuj od najnowszej do najstarszej wersji
        let pool = self.pool.borrow();
        solvables.sort_by(|&a, &b| {
            let va = &pool.resolve_solvable(a).version;
            let vb = &pool.resolve_solvable(b).version;
            // Odwrotna kolejność: najnowsze pierwsze
            cmp_versions_desc(vb, va)
        });
    }

    /// Zwraca zależności i konflikty dla konkretnego solvable
    async fn get_dependencies(&self, solvable: SolvableId) -> Dependencies {
        let deps = self.pool.borrow().get_package_deps(solvable);
        let conflicts = self.pool.borrow().get_package_conflicts(solvable);

        let mut requirements: Vec<Requirement> = Vec::new();
        let mut constrains: Vec<VersionSetId> = Vec::new();

        // Przetwórz zależności -> requirements
        for (dep_name, constraint) in deps {
            let name_id = self.pool.borrow_mut().intern_package_name(&dep_name);
            let vs_id = self.pool.borrow_mut().intern_version_set(name_id, constraint);
            requirements.push(vs_id.into());
        }

        // Przetwórz konflikty -> constrains (z odwróconym filtrem)
        for conflict_name in conflicts {
            let name_id = self.pool.borrow_mut().intern_package_name(&conflict_name);
            // Constraint ">= 0" + inverse=true oznacza "żaden z tych pakietów"
            let vs_id = self
                .pool
                .borrow_mut()
                .intern_version_set(name_id, ">= 0".to_string());
            constrains.push(vs_id);
        }

        Dependencies::Known(KnownDependencies {
            requirements,
            constrains,
        })
    }
}

/// Porównuje wersje dla sortowania malejącego
fn cmp_versions_desc(a: &str, b: &str) -> std::cmp::Ordering {
    use crate::pool::version_matches;

    // Użyj prostego string comparison dla wersji
    let a_parts: Vec<u64> = a
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|s| s.parse().ok())
        .collect();
    let b_parts: Vec<u64> = b
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|s| s.parse().ok())
        .collect();

    let max_len = a_parts.len().max(b_parts.len());
    for i in 0..max_len {
        let av = a_parts.get(i).copied().unwrap_or(0);
        let bv = b_parts.get(i).copied().unwrap_or(0);
        match av.cmp(&bv) {
            std::cmp::Ordering::Equal => continue,
            other => return other,
        }
    }
    std::cmp::Ordering::Equal
}