use std::collections::HashMap;
use indexmap::IndexMap;
use resolvo::{Candidates, NameId, SolvableId, VersionSetId};

/// Reprezentuje pojedynczy pakiet ALPM (jak w pacman/Arch)
#[derive(Debug, Clone)]
pub struct AlpmPackage {
    pub name: String,
    pub version: String,
    pub deps: Vec<AlpmDep>,
    pub provides: Vec<String>,    // np. "libpulse=17.0"
    pub conflicts: Vec<String>,   // np. "pulseaudio"
}

/// Zależność z opcjonalnym ograniczeniem wersji
#[derive(Debug, Clone)]
pub struct AlpmDep {
    pub name: String,
    pub constraint: String,  // np. ">= 3.90", "= 1.0", ">= 0" (dowolna)
}

/// Wewnętrzny solvable (instancja pakietu w puli)
#[derive(Debug, Clone)]
pub struct Solvable {
    pub name: String,
    pub version: String,
}

/// Pula pakietów - centralny rejestr dla resolvera
pub struct AlpmPool {
    // Internowanie nazw pakietów: nazwa -> NameId
    package_names: IndexMap<String, NameId>,

    // Wszystkie solvables (konkretne wersje pakietów)
    solvables: Vec<Solvable>,

    // Mapowanie: NameId -> lista SolvableId
    name_to_solvables: HashMap<u32, Vec<SolvableId>>,

    // Pakiety w repozytorium
    packages: Vec<AlpmPackage>,

    // Mapowanie: SolvableId -> AlpmPackage
    solvable_to_package: HashMap<u32, usize>,

    // Internowanie VersionSet: (NameId, constraint_string) -> VersionSetId
    version_sets: IndexMap<(u32, String), VersionSetId>,

    // Wirtualne pakiety (provides): virtual_name -> vec<real_package_name>
    virtuals: HashMap<String, Vec<String>>,

    next_name_id: u32,
    next_solvable_id: u32,
    next_vs_id: u32,
}

impl AlpmPool {
    pub fn new() -> Self {
        Self {
            package_names: IndexMap::new(),
            solvables: Vec::new(),
            name_to_solvables: HashMap::new(),
            packages: Vec::new(),
            solvable_to_package: HashMap::new(),
            version_sets: IndexMap::new(),
            virtuals: HashMap::new(),
            next_name_id: 0,
            next_solvable_id: 0,
            next_vs_id: 0,
        }
    }

    /// Rejestruje pakiet w puli
    pub fn add_package(&mut self, pkg: AlpmPackage) {
        let name = pkg.name.clone();
        let version = pkg.version.clone();

        let name_id = self.intern_package_name(&name);
        let solvable_id = SolvableId::from(self.next_solvable_id);
        self.next_solvable_id += 1;

        let pkg_idx = self.packages.len();
        self.packages.push(pkg);
        self.solvables.push(Solvable { name, version });

        self.solvable_to_package.insert(solvable_id.into(), pkg_idx);
        self.name_to_solvables
            .entry(name_id.into())
            .or_default()
            .push(solvable_id);
    }

    /// Rejestruje wirtualny pakiet (provides)
    pub fn add_virtual(&mut self, virtual_name: &str, _version: &str, provided_by: &str) {
        self.virtuals
            .entry(virtual_name.to_string())
            .or_default()
            .push(provided_by.to_string());
    }

    /// Internuje nazwę pakietu i zwraca jej NameId
    pub fn intern_package_name(&mut self, name: &str) -> NameId {
        if let Some(&id) = self.package_names.get(name) {
            return id;
        }
        let id = NameId::from(self.next_name_id);
        self.next_name_id += 1;
        self.package_names.insert(name.to_string(), id);
        id
    }

    /// Internuje VersionSet (name + constraint) i zwraca VersionSetId
    pub fn intern_version_set(&mut self, name_id: NameId, constraint: String) -> VersionSetId {
        let key = (name_id.into(), constraint.clone());
        if let Some(&id) = self.version_sets.get(&key) {
            return id;
        }
        let id = VersionSetId::from(self.next_vs_id);
        self.next_vs_id += 1;
        self.version_sets.insert(key, id);
        id
    }

    /// Pobiera kandydatów dla danej nazwy pakietu
    pub fn get_candidates_for(&self, name_id: NameId) -> Option<Candidates> {
        // Sprawdź najpierw bezpośrednie pakiety
        let direct: Vec<SolvableId> = self
            .name_to_solvables
            .get(&name_id.into())
            .cloned()
            .unwrap_or_default();

        // Sprawdź wirtualne (provides)
        let name_str = self.resolve_name(name_id)?;
        let mut all = direct;

        if let Some(providers) = self.virtuals.get(name_str) {
            for provider_name in providers {
                // Znajdź NameId dla dostawcy
                if let Some(&provider_name_id) = self.package_names.get(provider_name) {
                    if let Some(provider_solvables) =
                        self.name_to_solvables.get(&provider_name_id.into())
                    {
                        all.extend(provider_solvables.iter().copied());
                    }
                }
            }
        }

        if all.is_empty() {
            return None;
        }

        Some(Candidates {
            candidates: all,
            hint_dependencies_available: resolvo::HintDependenciesAvailable::All,
            locked: None,
            favored: None,
        })
    }

    /// Filtruje kandydatów wg ograniczenia wersji
    pub fn filter_by_version_set(
        &self,
        candidates: &[SolvableId],
        vs_id: VersionSetId,
        inverse: bool,
    ) -> Vec<SolvableId> {
        // Pobierz constraint string dla tego VersionSetId
        let constraint = self.resolve_version_set_constraint(vs_id);

        candidates
            .iter()
            .copied()
            .filter(|&solvable_id| {
                let solvable = &self.solvables[Into::<usize>::into(solvable_id)];
                let matches = version_matches(&solvable.version, &constraint);
                if inverse { !matches } else { matches }
            })
            .collect()
    }

    /// Zwraca zależności pakietu jako listę (name, constraint)
    pub fn get_package_deps(&self, solvable_id: SolvableId) -> Vec<(String, String)> {
        let pkg_idx = self
            .solvable_to_package
            .get(&solvable_id.into())
            .copied()
            .unwrap_or(usize::MAX);

        if pkg_idx == usize::MAX {
            return vec![];
        }

        self.packages[pkg_idx]
            .deps
            .iter()
            .map(|d| (d.name.clone(), d.constraint.clone()))
            .collect()
    }

    /// Zwraca konflikty pakietu
    pub fn get_package_conflicts(&self, solvable_id: SolvableId) -> Vec<String> {
        let pkg_idx = self
            .solvable_to_package
            .get(&solvable_id.into())
            .copied()
            .unwrap_or(usize::MAX);

        if pkg_idx == usize::MAX {
            return vec![];
        }

        self.packages[pkg_idx].conflicts.clone()
    }

    /// Rozwiązuje NameId -> nazwa pakietu
    pub fn resolve_name(&self, name_id: NameId) -> Option<&str> {
        self.package_names
            .iter()
            .find(|(_, &v)| v == name_id)
            .map(|(k, _)| k.as_str())
    }

    /// Rozwiązuje SolvableId -> Solvable
    pub fn resolve_solvable(&self, solvable_id: SolvableId) -> &Solvable {
        &self.solvables[Into::<usize>::into(solvable_id)]
    }

    /// Zwraca constraint string dla danego VersionSetId
    pub fn resolve_version_set_constraint(&self, vs_id: VersionSetId) -> String {
        self.version_sets
            .iter()
            .find(|(_, &v)| v == vs_id)
            .map(|((_, c), _)| c.clone())
            .unwrap_or_else(|| ">= 0".to_string())
    }

    /// Zwraca NameId dla danego VersionSetId
    pub fn resolve_version_set_name(&self, vs_id: VersionSetId) -> Option<NameId> {
        self.version_sets
            .iter()
            .find(|(_, &v)| v == vs_id)
            .map(|((name_id, _), _)| NameId::from(*name_id))
    }

    pub fn package_names(&self) -> &IndexMap<String, NameId> {
        &self.package_names
    }

    pub fn name_to_solvables(&self) -> &HashMap<u32, Vec<SolvableId>> {
        &self.name_to_solvables
    }

    pub fn solvables_len(&self) -> usize {
        self.solvables.len()
    }
}

/// Porównuje wersję ALPM z ograniczeniem
/// Obsługuje: >= X, <= X, > X, < X, = X, >= 0 (dowolna)
pub fn version_matches(pkg_version: &str, constraint: &str) -> bool {
    let constraint = constraint.trim();

    if constraint == ">= 0" || constraint.is_empty() {
        return true;
    }

    let (op, ver) = if let Some(v) = constraint.strip_prefix(">=") {
        (">=", v.trim())
    } else if let Some(v) = constraint.strip_prefix("<=") {
        ("<=", v.trim())
    } else if let Some(v) = constraint.strip_prefix('>') {
        (">", v.trim())
    } else if let Some(v) = constraint.strip_prefix('<') {
        ("<", v.trim())
    } else if let Some(v) = constraint.strip_prefix('=') {
        ("=", v.trim())
    } else {
        ("=", constraint)
    };

    let cmp = compare_alpm_versions(pkg_version, ver);

    match op {
        ">=" => cmp >= 0,
        "<=" => cmp <= 0,
        ">"  => cmp > 0,
        "<"  => cmp < 0,
        "="  => cmp == 0,
        _    => true,
    }
}

/// Porównuje dwie wersje w formacie ALPM (uproszczone)
/// Zwraca: -1 (mniej), 0 (równo), 1 (więcej)
fn compare_alpm_versions(a: &str, b: &str) -> i32 {
    // Usuń epokę (np. "2:")
    let a = strip_epoch(a);
    let b = strip_epoch(b);

    // Usuń release (po "-")
    let (a_ver, _a_rel) = split_version_release(a);
    let (b_ver, _b_rel) = split_version_release(b);

    compare_version_string(a_ver, b_ver)
}

fn strip_epoch(v: &str) -> &str {
    if let Some(pos) = v.find(':') {
        &v[pos + 1..]
    } else {
        v
    }
}

fn split_version_release(v: &str) -> (&str, &str) {
    if let Some(pos) = v.rfind('-') {
        (&v[..pos], &v[pos + 1..])
    } else {
        (v, "")
    }
}

fn compare_version_string(a: &str, b: &str) -> i32 {
    let a_parts: Vec<u64> = a.split('.').filter_map(|s| s.parse().ok()).collect();
    let b_parts: Vec<u64> = b.split('.').filter_map(|s| s.parse().ok()).collect();

    let max_len = a_parts.len().max(b_parts.len());
    for i in 0..max_len {
        let av = a_parts.get(i).copied().unwrap_or(0);
        let bv = b_parts.get(i).copied().unwrap_or(0);
        if av < bv { return -1; }
        if av > bv { return 1; }
    }
    0
}