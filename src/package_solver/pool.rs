use indexmap::IndexMap;
use std::cell::RefCell;
use std::collections::HashMap;
use resolvo::{NameId, SolvableId, VersionSetId, VersionSetUnionId, Candidates, HintDependenciesAvailable};

// ──────────────────────────────────────────────
// Typy danych
// ──────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct AlpmPackage {
    pub name:      String,
    pub version:   String,
    pub pkgrel:    String,
    pub repo:      String,
    pub size:      u64,
    pub deps:      Vec<AlpmDep>,
    pub provides:  Vec<AlpmProvide>,
    pub conflicts: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct AlpmDep {
    pub name:       String,
    /// Pusty string = brak constraintu (dowolna wersja)
    /// Inaczej: ">= 1.0", "= 2.3", "< 4.0" itd.
    pub constraint: String,
}

#[derive(Debug, Clone)]
pub struct AlpmProvide {
    pub virtual_name:    String,
    pub virtual_version: String,
}

#[derive(Debug, Clone)]
pub struct Solvable {
    pub name:            String,
    pub version:         String,
    pub repo:            String,
    /// Dla virtual provides: wersja provide zamiast wersji pakietu
    pub provide_version: Option<String>,
}

// ──────────────────────────────────────────────
// Pula pakietów
// ──────────────────────────────────────────────

pub struct AlpmPool {
    // Internowanie (RefCell bo DependencyProvider wymaga &self)
    name_to_id: RefCell<IndexMap<String, u32>>,
    vs_map:     RefCell<IndexMap<(u32, String), u32>>,
    unions:     RefCell<Vec<Vec<VersionSetId>>>,

    // Dane tylko do odczytu po setup
    solvables:       Vec<Solvable>,
    solvable_to_pkg: Vec<usize>,
    packages:        Vec<AlpmPackage>,
    name_candidates: HashMap<u32, Vec<SolvableId>>,

    /// virtual_name -> [(pkg_name, provide_version, solvable_id)]
    pub virtuals:     HashMap<String, Vec<(String, String, SolvableId)>>,
    pub pkg_provides: Vec<Vec<AlpmProvide>>,

    /// Graf konfliktów: (name_id_A, name_id_B)
    /// Symetryczny — każda krawędź jest dodana w obu kierunkach
    pub conflict_graph: Vec<(u32, u32)>,
}

impl AlpmPool {
    pub fn new() -> Self {
        Self {
            name_to_id:     RefCell::new(IndexMap::new()),
            vs_map:         RefCell::new(IndexMap::new()),
            unions:         RefCell::new(Vec::new()),
            solvables:      Vec::new(),
            solvable_to_pkg: Vec::new(),
            packages:       Vec::new(),
            name_candidates: HashMap::new(),
            virtuals:       HashMap::new(),
            pkg_provides:   Vec::new(),
            conflict_graph: Vec::new(),
        }
    }

    // ── Internowanie nazw ───────────────────────────────────────────────────

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

    // ── Internowanie VersionSet ─────────────────────────────────────────────

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

    /// Zwraca (NameId, constraint) jako owned — bez pożyczania z RefCell
    pub fn resolve_version_set(&self, id: VersionSetId) -> (NameId, String) {
        let map = self.vs_map.borrow();
        let ((name_id, constraint), _) = map
            .get_index(id.0 as usize)
            .expect("invalid VersionSetId");
        (NameId(*name_id), constraint.clone())
    }

    // ── VersionSetUnion ─────────────────────────────────────────────────────

    pub fn intern_union(&self, sets: Vec<VersionSetId>) -> VersionSetUnionId {
        let mut u = self.unions.borrow_mut();
        let id = u.len() as u32;
        u.push(sets);
        VersionSetUnionId(id)
    }

    pub fn resolve_union(&self, id: VersionSetUnionId) -> Vec<VersionSetId> {
        self.unions.borrow()[id.0 as usize].clone()
    }

    // ── Dodawanie pakietów (tylko &mut self przy setup) ─────────────────────

    pub fn add_package(&mut self, pkg: AlpmPackage) {
        let name_id     = self.intern_name(&pkg.name.clone());
        let solvable_id = SolvableId(self.solvables.len() as u32);

        self.solvables.push(Solvable {
            name:            pkg.name.clone(),
            version:         pkg.version.clone(),
            repo:            pkg.repo.clone(),
            provide_version: None,
        });
        self.solvable_to_pkg.push(self.packages.len());
        self.name_candidates
            .entry(name_id.0)
            .or_default()
            .push(solvable_id);

        // Virtual provides
        let pkg_provides = pkg.provides.clone();
        self.pkg_provides.push(pkg_provides.clone());

        for provide in &pkg.provides {
            self.virtuals
                .entry(provide.virtual_name.clone())
                .or_default()
                .push((
                    pkg.name.clone(),
                    provide.virtual_version.clone(),
                    solvable_id,
                ));
        }

        // Graf konfliktów — symetryczny, pomijaj self-konflikty
        for conflict in &pkg.conflicts {
            // Pomiń jeśli pakiet konfliktuje sam ze sobą (np. przez provides)
            if conflict == &pkg.name {
                continue;
            }
            let conflict_name_id = self.intern_name(conflict);
            let pair_fwd = (name_id.0, conflict_name_id.0);
            let pair_rev = (conflict_name_id.0, name_id.0);
            if !self.conflict_graph.contains(&pair_fwd) {
                self.conflict_graph.push(pair_fwd);
            }
            if !self.conflict_graph.contains(&pair_rev) {
                self.conflict_graph.push(pair_rev);
            }
        }

        self.packages.push(pkg);
    }

    /// Wywołaj po załadowaniu wszystkich pakietów —
    /// ustawia provide_version dla virtual solvables
    pub fn finalize_virtuals(&mut self) {
        let entries: Vec<_> = self.virtuals
            .values()
            .flat_map(|providers| providers.iter().cloned())
            .collect();

        for (_, provide_version, solvable_id) in entries {
            if solvable_id.0 < self.solvables.len() as u32 {
                // Ustaw tylko jeśli jeszcze nie ustawione (pierwszy provide wygrywa)
                if self.solvables[solvable_id.0 as usize].provide_version.is_none() {
                    self.solvables[solvable_id.0 as usize].provide_version =
                        Some(provide_version);
                }
            }
        }
    }

    /// Scal pakiety z local puli (zainstalowane w dest) do tej puli
    /// Używane żeby solver wiedział co jest już zainstalowane
    pub fn merge_local(&mut self, local_pool: AlpmPool) {
        // Wyciągnij pakiety z local_pool i dodaj je do self
        for pkg in local_pool.packages {
            // Sprawdź czy już nie ma pakietu o tej nazwie w local
            let already_local = self.packages.iter()
                .any(|p| p.name == pkg.name && p.repo == "local");
            if !already_local {
                self.add_package(pkg);
            }
        }
        self.finalize_virtuals();
    }

    // ── Zapytania ───────────────────────────────────────────────────────────

    pub fn resolve_solvable(&self, id: SolvableId) -> &Solvable {
        &self.solvables[id.0 as usize]
    }

    pub fn solvable_name_id(&self, id: SolvableId) -> NameId {
        let pkg_idx = self.solvable_to_pkg[id.0 as usize];
        let name    = &self.packages[pkg_idx].name;
        NameId(*self.name_to_id.borrow().get(name).unwrap())
    }

    pub fn get_candidates_for(&self, name: NameId) -> Option<Candidates> {
        let name_str = self.resolve_name(name);

        let mut all: Vec<SolvableId> = self
            .name_candidates
            .get(&name.0)
            .cloned()
            .unwrap_or_default();

        if let Some(providers) = self.virtuals.get(&name_str) {
            for (_, _, sid) in providers {
                if !all.contains(sid) {
                    all.push(*sid);
                }
            }
        }

        if all.is_empty() {
            return None;
        }

        let locked = all.iter().copied()
            .find(|&sid| self.solvables[sid.0 as usize].repo == "local");

        // Jeśli pakiet jest zainstalowany — wyklucz wersje z sync DB
        // które mają inną wersję niż zainstalowana
        let candidates = if locked.is_some() {
            all.iter().copied()
            .filter(|&sid| self.solvables[sid.0 as usize].repo == "local")
            .collect()
        } else {
            all
        };

        let favored = if locked.is_some() {
            locked
        } else {
            self.get_favored_solvable(&candidates)
        };

        Some(Candidates {
            candidates,
            hint_dependencies_available: HintDependenciesAvailable::All,
            locked,
            favored,
            excluded: vec![],
        })
    }

    pub fn filter_candidates(
        &self,
        candidates: &[SolvableId],
        vs: VersionSetId,
        inverse: bool,
    ) -> Vec<SolvableId> {
        let (name_id, constraint) = self.resolve_version_set(vs);
        let query_name = self.resolve_name(name_id);

        candidates.iter().copied().filter(|&sid| {
        let solvable = &self.solvables[sid.0 as usize];
        let pkg_idx  = self.solvable_to_pkg[sid.0 as usize];
        let pkg      = &self.packages[pkg_idx];

        // Wybierz właściwą wersję do porównania:
        // - jeśli zapytanie jest o własną nazwę pakietu → wersja pakietu
        // - jeśli zapytanie jest o provide → wersja tego provide
        let ver_to_cmp = if solvable.name == query_name {
            pkg.version.clone()
        } else {
            pkg.provides.iter()
                .find(|p| p.virtual_name == query_name)
                .map(|p| p.virtual_version.clone())
                .unwrap_or_else(|| pkg.version.clone())
        };

        let ok = version_matches(&ver_to_cmp, &constraint);
        if inverse { !ok } else { ok }
        }).collect()
    }

    pub fn get_deps(&self, id: SolvableId) -> &[AlpmDep] {
        &self.packages[self.solvable_to_pkg[id.0 as usize]].deps
    }

    pub fn get_conflicts(&self, id: SolvableId) -> &[String] {
        &self.packages[self.solvable_to_pkg[id.0 as usize]].conflicts
    }

    pub fn get_package_name(&self, id: SolvableId) -> &str {
        &self.packages[self.solvable_to_pkg[id.0 as usize]].name
    }

    pub fn get_package_size(&self, id: SolvableId) -> Option<u64> {
        let idx = self.solvable_to_pkg.get(id.0 as usize)?;
        Some(self.packages.get(*idx)?.size)
    }

    pub fn get_package_pkgrel(&self, id: SolvableId) -> Option<String> {
        let idx = self.solvable_to_pkg.get(id.0 as usize)?;
        Some(self.packages.get(*idx)?.pkgrel.clone())
    }

    pub fn package_count(&self) -> usize {
        self.packages.len()
    }

    pub fn get_installed_packages(&self) -> Vec<(SolvableId, &Solvable)> {
        self.solvables
            .iter()
            .enumerate()
            .filter(|(_, s)| s.repo == "local")
            .map(|(i, s)| (SolvableId(i as u32), s))
            .collect()
    }

    // ── Konflikty ───────────────────────────────────────────────────────────

    pub fn conflicts(&self, name_a: NameId, name_b: NameId) -> bool {
        self.conflict_graph.iter().any(|&(a, b)| {
            (a == name_a.0 && b == name_b.0) || (a == name_b.0 && b == name_a.0)
        })
    }

    pub fn get_conflicting_with(&self, name_id: NameId) -> Vec<String> {
        let mut result: std::collections::HashSet<String> = Default::default();
        for &(a, b) in &self.conflict_graph {
            if a == name_id.0 {
                result.insert(self.resolve_name(NameId(b)));
            } else if b == name_id.0 {
                result.insert(self.resolve_name(NameId(a)));
            }
        }
        result.into_iter().collect()
    }

    // ── Priorytet repo i faworyzowanie ─────────────────────────────────────

    fn repo_priority(repo: &str) -> u32 {
        match repo {
            "core"               => 0,
            "extra"              => 1,
            "multilib"           => 1,
            "community"          => 2,
            "community-testing"  => 3,
            "testing"            => 4,
            _                    => u32::MAX,
        }
    }

    fn get_favored_solvable(&self, candidates: &[SolvableId]) -> Option<SolvableId> {
        if candidates.is_empty() {
            return None;
        }

        // Wybierz najwyższy priorytet repo (najniższa liczba)
        let best_prio = candidates.iter()
            .map(|&sid| Self::repo_priority(&self.solvables[sid.0 as usize].repo))
            .min()
            .unwrap_or(u32::MAX);

        let mut by_repo: Vec<SolvableId> = candidates.iter()
            .filter(|&&sid| {
                Self::repo_priority(&self.solvables[sid.0 as usize].repo) == best_prio
            })
            .copied()
            .collect();

        if by_repo.is_empty() {
            return None;
        }

        // Spośród nich wybierz najnowszą wersję
        by_repo.sort_by(|&a, &b| {
            let va = &self.solvables[a.0 as usize].version;
            let vb = &self.solvables[b.0 as usize].version;
            match alpm::vercmp(vb.as_bytes(), va.as_bytes()) {
                std::cmp::Ordering::Less    => std::cmp::Ordering::Greater,
                std::cmp::Ordering::Equal   => std::cmp::Ordering::Equal,
                std::cmp::Ordering::Greater => std::cmp::Ordering::Less,
            }
        });

        by_repo.first().copied()
    }
}

// ──────────────────────────────────────────────
// Porównywanie wersji przez libalpm
// ──────────────────────────────────────────────

pub fn version_matches(pkg_ver: &str, constraint: &str) -> bool {
    let c = constraint.trim();

    // Pusty constraint = dowolna wersja
    if c.is_empty() {
        return true;
    }

    let (op, req) = if let Some(v) = c.strip_prefix(">=") {
        (">=", v.trim())
    } else if let Some(v) = c.strip_prefix("<=") {
        ("<=", v.trim())
    } else if let Some(v) = c.strip_prefix('>') {
        (">", v.trim())
    } else if let Some(v) = c.strip_prefix('<') {
        ("<", v.trim())
    } else if let Some(v) = c.strip_prefix('=') {
        ("=", v.trim())
    } else {
        // Brak operatora = dokładna równość
        ("=", c)
    };

    if req.is_empty() {
        return true;
    }

    let cmp = match alpm::vercmp(pkg_ver, req) {
        std::cmp::Ordering::Less    => -1,
        std::cmp::Ordering::Equal   =>  0,
        std::cmp::Ordering::Greater =>  1,
    };

    match op {
        ">=" => cmp >= 0,
        "<=" => cmp <= 0,
        ">"  => cmp > 0,
        "<"  => cmp < 0,
        _    => cmp == 0,
    }
}