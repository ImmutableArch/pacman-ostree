use resolvo::{
    Candidates, Condition, ConditionId, Dependencies, DependencyProvider,
    Interner, KnownDependencies, NameId, ConditionalRequirement,
    Requirement, SolvableId, SolverCache, VersionSetId, VersionSetUnionId,
};

use crate::package_solver::AlpmPool;
use std::rc::Rc;

// ──────────────────────────────────────────────
// Provider
//
// AlpmPool jest owinięty w Rc aby móc być współdzielony między
// PackageManager'em i Provider'em bez klonowania
// ──────────────────────────────────────────────

pub struct AlpmDependencyProvider {
    pool: Rc<AlpmPool>,
}

impl AlpmDependencyProvider {
    pub fn new(pool: Rc<AlpmPool>) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &AlpmPool {
        &self.pool
    }

    /// Sprawdź czy dwie requirements konfliktują - vracia conflicts jeśli znaleziony
    pub fn validate_requirements(
        &self,
        requirement_names: &[NameId],
    ) -> Result<(), (NameId, NameId, String)> {
        for i in 0..requirement_names.len() {
            for j in (i + 1)..requirement_names.len() {
                let name_a = requirement_names[i];
                let name_b = requirement_names[j];
                if self.pool.conflicts(name_a, name_b) {
                    let name_a_str = self.pool.resolve_name(name_a);
                    let name_b_str = self.pool.resolve_name(name_b);
                    return Err((
                        name_a,
                        name_b,
                        format!("'{}' conflicts with '{}'", name_a_str, name_b_str),
                    ));
                }
            }
        }
        Ok(())
    }
}

// ──────────────────────────────────────────────
// Interner
// ──────────────────────────────────────────────

impl Interner for AlpmDependencyProvider {
    fn display_solvable(&self, solvable: SolvableId) -> impl std::fmt::Display + '_ {
        let s = self.pool.resolve_solvable(solvable);
        format!("{}-{}", s.name, s.version)
    }

    fn display_name(&self, name: NameId) -> impl std::fmt::Display + '_ {
        // resolve_name zwraca String, więc brak problemu z lifetime
        self.pool.resolve_name(name)
    }

    fn display_version_set(&self, version_set: VersionSetId) -> impl std::fmt::Display + '_ {
        let (_, constraint) = self.pool.resolve_version_set(version_set);
        constraint
    }

    fn display_string(&self, _string_id: resolvo::StringId) -> impl std::fmt::Display + '_ {
        "<str>"
    }

    fn version_set_name(&self, version_set: VersionSetId) -> NameId {
        let (name_id, _) = self.pool.resolve_version_set(version_set);
        name_id
    }

    fn solvable_name(&self, solvable: SolvableId) -> NameId {
        self.pool.solvable_name_id(solvable)
    }

    fn version_sets_in_union(
        &self,
        version_set_union: VersionSetUnionId,
    ) -> impl Iterator<Item = VersionSetId> {
        // resolve_union zwraca Vec (owned), iterator jest niezależny od borrows
        self.pool.resolve_union(version_set_union).into_iter()
    }

    fn resolve_condition(&self, _condition: ConditionId) -> Condition {
        // Nie używamy conditional dependencies
        Condition::Requirement(VersionSetId(0))
    }
}

// ──────────────────────────────────────────────
// DependencyProvider
// ──────────────────────────────────────────────

impl DependencyProvider for AlpmDependencyProvider {
    async fn filter_candidates(
        &self,
        candidates: &[SolvableId],
        version_set: VersionSetId,
        inverse: bool,
    ) -> Vec<SolvableId> {
        self.pool.filter_candidates(candidates, version_set, inverse)
    }

    async fn get_candidates(&self, name: NameId) -> Option<Candidates> {
        self.pool.get_candidates_for(name)
    }

    async fn sort_candidates(
        &self,
        _solver: &SolverCache<Self>,
        solvables: &mut [SolvableId],
    ) {
        solvables.sort_by(|&a, &b| {
            let va = &self.pool.resolve_solvable(a).version;
            let vb = &self.pool.resolve_solvable(b).version;
            // Sortuj od najnowszej do najstarszej (descending)
            match alpm::vercmp(vb.as_bytes(), va.as_bytes()) {
                std::cmp::Ordering::Less => std::cmp::Ordering::Greater,
                std::cmp::Ordering::Equal => std::cmp::Ordering::Equal,
                std::cmp::Ordering::Greater => std::cmp::Ordering::Less,
            }
        });
    }

    async fn get_dependencies(&self, solvable: SolvableId) -> Dependencies {
        let deps      = self.pool.get_deps(solvable).to_vec();
        let conflicts = self.pool.get_conflicts(solvable).to_vec();

        let mut requirements: Vec<ConditionalRequirement> = Vec::new();
        let mut constrains:   Vec<VersionSetId> = Vec::new();

        // Dodaj zależności
        for dep in &deps {
            let name_id = self.pool.intern_name(&dep.name);
            let vs_id   = self.pool.intern_version_set(name_id, &dep.constraint);
            requirements.push(Requirement::Single(vs_id).into());
        }

        // Dodaj konflikty jako constraints negacyjne
        // Konflikt: jeśli A jest zainstalowany, B musi być wykluczone
        // Modelujemy to poprzez "version set" który będzie zawierał wszystkie wersje pakietu konfliktującego
        for conflict in &conflicts {
            let name_id = self.pool.intern_name(conflict);
            // ">= 0" oznacza dowolną wersję - to będzie constraint który wymaga że
            // żaden pakiet o tej nazwie nie może być zainstalowany
            let vs_id   = self.pool.intern_version_set(name_id, ">= 0");
            constrains.push(vs_id);
        }

        Dependencies::Known(KnownDependencies { requirements, constrains })
    }
}