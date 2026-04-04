use indexmap::IndexMap;
use std::cell::RefCell;
use std::collections::HashMap;
use resolvo::{NameId, SolvableId, VersionSetId, VersionSetUnionId, Candidates, HintDependenciesAvailable};

// ──────────────────────────────────────────────
// Typy danych ALPM
// ──────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct AlpmPackage {
    pub name:      String,
    pub version:   String,
    pub pkgrel:    String,  // Package release number
    pub repo:      String,  // "core", "extra", "community", etc.
    pub size:      u64,     // Rozmiar pakietu w bajtach
    pub deps:      Vec<AlpmDep>,
    pub provides:  Vec<AlpmProvide>,
    pub conflicts: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct AlpmDep {
    pub name:       String,
    pub constraint: String,
}

#[derive(Debug, Clone)]
pub struct AlpmProvide {
    pub virtual_name:    String,
    pub virtual_version: String,
}

#[derive(Debug, Clone)]
pub struct Solvable {
    pub name:    String,
    pub version: String,
    pub repo:    String,
}

// ──────────────────────────────────────────────
// Pula pakietów
//
// Dane niezmienne (packages, solvables, candidates) są bezpośrednio dostępne
// przez &self. Dane wymagające mutacji przy zapytaniach (internowanie nazw
// i version-setów) są osłonięte RefCell, co pozwala na &self w metodach
// DependencyProvider.
// ──────────────────────────────────────────────

pub struct AlpmPool {
    // -- Internowanie (wymaga mut przy zapytaniach) --
    name_to_id: RefCell<IndexMap<String, u32>>,
    vs_map:     RefCell<IndexMap<(u32, String), u32>>,
    unions:     RefCell<Vec<Vec<VersionSetId>>>,

    // -- Dane tylko do odczytu po setup --
    solvables:       Vec<Solvable>,
    solvable_to_pkg: Vec<usize>,
    packages:        Vec<AlpmPackage>,
    name_candidates: HashMap<u32, Vec<SolvableId>>,
    /// "libpulse" -> [(provider_name, provider_version, provider_solvable), ...]
    pub virtuals:    HashMap<String, Vec<(String, String, SolvableId)>>,
    /// Reverse mapping: package_name -> provides (dla debugowania i validation)
    pub pkg_provides: Vec<Vec<AlpmProvide>>,
    /// Global conflict graph: (name_id_A, name_id_B) - pakiety które konfliktują
    pub conflict_graph: Vec<(u32, u32)>,
}

impl AlpmPool {
    pub fn new() -> Self {
        Self {
            name_to_id:      RefCell::new(IndexMap::new()),
            vs_map:          RefCell::new(IndexMap::new()),
            unions:          RefCell::new(Vec::new()),
            solvables:       Vec::new(),
            solvable_to_pkg: Vec::new(),
            packages:        Vec::new(),
            name_candidates: HashMap::new(),
            virtuals:        HashMap::new(),
            pkg_provides:    Vec::new(),
            conflict_graph:  Vec::new(),
        }
    }

    // ── Nazwy (&self — RefCell) ─────────────────

    pub fn intern_name(&self, name: &str) -> NameId {
        let mut map = self.name_to_id.borrow_mut();
        if let Some(&id) = map.get(name) {
            return NameId(id);
        }
        let id = map.len() as u32;
        map.insert(name.to_string(), id);
        NameId(id)
    }

    pub fn lookup_name(&self, name: &str) -> Option<NameId> {
        self.name_to_id.borrow().get(name).map(|&id| NameId(id))
    }

    pub fn resolve_name(&self, id: NameId) -> String {
        self.name_to_id.borrow()
            .get_index(id.0 as usize)
            .map(|(k, _)| k.clone())
            .unwrap_or_else(|| "<unknown>".to_string())
    }

    // ── VersionSet (&self — RefCell) ────────────

    pub fn intern_version_set(&self, name: NameId, constraint: &str) -> VersionSetId {
        let key = (name.0, constraint.to_string());
        let mut map = self.vs_map.borrow_mut();
        if let Some(&id) = map.get(&key) {
            return VersionSetId(id);
        }
        let id = map.len() as u32;
        map.insert(key, id);
        VersionSetId(id)
    }

    /// Zwraca (NameId, constraint_string) — jako owned, bez &str pożyczki z RefCell
    pub fn resolve_version_set(&self, id: VersionSetId) -> (NameId, String) {
        let map = self.vs_map.borrow();
        let ((name_id, constraint), _) = map.get_index(id.0 as usize)
            .expect("invalid VersionSetId");
        (NameId(*name_id), constraint.clone())
    }

    // ── VersionSetUnion (&self — RefCell) ───────

    pub fn intern_union(&self, sets: Vec<VersionSetId>) -> VersionSetUnionId {
        let mut u = self.unions.borrow_mut();
        let id = u.len() as u32;
        u.push(sets);
        VersionSetUnionId(id)
    }

    pub fn resolve_union(&self, id: VersionSetUnionId) -> Vec<VersionSetId> {
        self.unions.borrow()[id.0 as usize].clone()
    }

    // ── Pakiety / solvables (tylko &mut self przy setup) ──

    pub fn add_package(&mut self, pkg: AlpmPackage) {
        let name_id = self.intern_name(&pkg.name.clone());
        let solvable_id = SolvableId(self.solvables.len() as u32);
        self.solvables.push(Solvable { 
            name: pkg.name.clone(), 
            version: pkg.version.clone(),
            repo: pkg.repo.clone(),
        });
        self.solvable_to_pkg.push(self.packages.len());
        self.name_candidates.entry(name_id.0).or_default().push(solvable_id);

        // Track provides per-package
        let pkg_provides = pkg.provides.clone();
        self.pkg_provides.push(pkg_provides.clone());

        // Dodaj virtual provides
        for provide in &pkg.provides {
            self.virtuals
                .entry(provide.virtual_name.clone())
                .or_default()
                .push((pkg.name.clone(), provide.virtual_version.clone(), solvable_id));
        }

        // Kompiluj conflict graph (global)
        for conflict in &pkg.conflicts {
            let conflict_name_id = self.intern_name(conflict);
            // Dodaj conflict obustronnie
            self.conflict_graph.push((name_id.0, conflict_name_id.0));
            self.conflict_graph.push((conflict_name_id.0, name_id.0));
        }

        self.packages.push(pkg);
    }

    pub fn add_virtual_provides(&mut self, virtual_name: &str, provider_name: &str, provider_version: &str) {
        // DEPRECATED - maintainer should use AlpmProvide in package.provides
        // This is for backwards compatibility only
        if let Some(pkg) = self.packages.iter_mut().find(|p| p.name == provider_name && p.version == provider_version) {
            pkg.provides.push(AlpmProvide {
                virtual_name: virtual_name.to_string(),
                virtual_version: provider_version.to_string(),
            });
        }
    }

    // ── Zapytania (&self) ───────────────────────

    pub fn resolve_solvable(&self, id: SolvableId) -> &Solvable {
        &self.solvables[id.0 as usize]
    }

    pub fn solvable_name_id(&self, id: SolvableId) -> NameId {
        let pkg_idx = self.solvable_to_pkg[id.0 as usize];
        let name = &self.packages[pkg_idx].name;
        NameId(*self.name_to_id.borrow().get(name).unwrap())
    }

    pub fn get_candidates_for(&self, name: NameId) -> Option<Candidates> {
        let name_str = self.resolve_name(name);
        let mut all: Vec<SolvableId> = self
            .name_candidates.get(&name.0).cloned().unwrap_or_default();

        if let Some(providers) = self.virtuals.get(&name_str) {
            for (_, _, solvable_id) in providers {
                if !all.contains(solvable_id) { 
                    all.push(*solvable_id); 
                }
            }
        }

        if all.is_empty() { return None; }

        // Ustaw favored na podstawie repo priority
        // Wybiż solvable z najwyższym priorytetem repo i najnowszą wersją
        let favored = self.get_favored_solvable(&all);

        Some(Candidates {
            candidates: all,
            hint_dependencies_available: HintDependenciesAvailable::All,
            locked: None,
            favored,
            excluded: vec![],
        })
    }

    /// Sprawdź czy dwie package names konfliktują
    pub fn conflicts(&self, name_a: NameId, name_b: NameId) -> bool {
        self.conflict_graph.iter().any(|&(a, b)| (a == name_a.0 && b == name_b.0) || (a == name_b.0 && b == name_a.0))
    }

    /// Collect all packages that konfliktują z podanym package name
    pub fn get_conflicting_with(&self, name_id: NameId) -> Vec<String> {
        self.conflict_graph
            .iter()
            .filter_map(|&(a, b)| {
                if a == name_id.0 {
                    Some(self.resolve_name(NameId(b)))
                } else if b == name_id.0 {
                    Some(self.resolve_name(NameId(a)))
                } else {
                    None
                }
            })
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect()
    }

    fn get_repo_priority(repo: &str) -> u32 {
        match repo {
            "core" => 0,
            "extra" => 1,
            "multilib" => 1,
            "community" => 2,
            "community-testing" => 3,
            "testing" => 4,
            _ => u32::MAX,
        }
    }

    /// Wybierz jedno solvable z najwyższym priorytetem repo (i najnowszą wersją)
    fn get_favored_solvable(&self, candidates: &[SolvableId]) -> Option<SolvableId> {
        if candidates.is_empty() { return None; }

        // Najpierw filtruj po prioritecie repo
        let min_priority = candidates
            .iter()
            .map(|&sid| Self::get_repo_priority(&self.solvables[sid.0 as usize].repo))
            .min()
            .unwrap_or(u32::MAX);

        let mut favored_by_repo: Vec<SolvableId> = candidates
            .iter()
            .filter(|&&sid| Self::get_repo_priority(&self.solvables[sid.0 as usize].repo) == min_priority)
            .copied()
            .collect();

        if favored_by_repo.is_empty() { return None; }

        // Teraz wybiż najnowszą wersję spośród nich
        favored_by_repo.sort_by(|&a, &b| {
            let version_a = &self.solvables[a.0 as usize].version;
            let version_b = &self.solvables[b.0 as usize].version;
            match alpm::vercmp(version_b.as_bytes(), version_a.as_bytes()) {
                std::cmp::Ordering::Less => std::cmp::Ordering::Greater,
                std::cmp::Ordering::Equal => std::cmp::Ordering::Equal,
                std::cmp::Ordering::Greater => std::cmp::Ordering::Less,
            }
        });

        favored_by_repo.first().copied()
    }

    pub fn filter_candidates(
        &self,
        candidates: &[SolvableId],
        vs: VersionSetId,
        inverse: bool,
    ) -> Vec<SolvableId> {
        let (_, constraint) = self.resolve_version_set(vs);
        candidates.iter().copied().filter(|&sid| {
            let ok = version_matches(&self.solvables[sid.0 as usize].version, &constraint);
            if inverse { !ok } else { ok }
        }).collect()
    }

    pub fn get_deps(&self, id: SolvableId) -> &[AlpmDep] {
        &self.packages[self.solvable_to_pkg[id.0 as usize]].deps
    }

    pub fn get_conflicts(&self, id: SolvableId) -> &[String] {
        &self.packages[self.solvable_to_pkg[id.0 as usize]].conflicts
    }

    /// Pobierz rozmiar pakietu z danego solvable
    pub fn get_package_size(&self, id: SolvableId) -> Option<u64> {
        let pkg_idx = self.solvable_to_pkg.get(id.0 as usize)?;
        Some(self.packages.get(*pkg_idx)?.size)
    }

    /// Pobierz pkgrel pakietu z danego solvable
    pub fn get_package_pkgrel(&self, id: SolvableId) -> Option<String> {
        let pkg_idx = self.solvable_to_pkg.get(id.0 as usize)?;
        Some(self.packages.get(*pkg_idx)?.pkgrel.clone())
    }
}

// ──────────────────────────────────────────────
// Porównywanie wersji ALPM (z libalpm)
// ──────────────────────────────────────────────

pub fn version_matches(pkg_ver: &str, constraint: &str) -> bool {
    let c = constraint.trim();
    if c.is_empty() || c == ">= 0" { return true; }

    let (op, req) = if let Some(v) = c.strip_prefix(">=") { (">=", v.trim()) }
    else if let Some(v) = c.strip_prefix("<=") { ("<=", v.trim()) }
    else if let Some(v) = c.strip_prefix('>') { (">", v.trim()) }
    else if let Some(v) = c.strip_prefix('<') { ("<", v.trim()) }
    else if let Some(v) = c.strip_prefix('=') { ("=", v.trim()) }
    else { ("=", c) };

    let cmp = compare_versions(pkg_ver, req);
    match op {
        ">=" => cmp >= 0,
        "<=" => cmp <= 0,
        ">"  => cmp > 0,
        "<"  => cmp < 0,
        _    => cmp == 0,
    }
}

fn compare_versions(a: &str, b: &str) -> i32 {
    // Używaj vercmp z libalpm dla poprawnej semantyki wersji Arch Linux
    // (obsługuje epoch, pkgrel, rc/alpha/beta suffixes, itd.)
    match alpm::vercmp(a, b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}