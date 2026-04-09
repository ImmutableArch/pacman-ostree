use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use anyhow::Context;

#[derive(Debug, Clone, PartialEq)]
pub enum HookWhen {
    PreTransaction,
    PostTransaction,
}

#[derive(Debug, Clone, PartialEq)]
pub enum HookTriggerType {
    File,
    Package,
    Path,
}

#[derive(Debug, Clone, PartialEq)]
pub enum HookOperation {
    Install,
    Upgrade,
    Remove,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HookTrigger {
    pub trigger_type: HookTriggerType,
    pub operations:   Vec<HookOperation>,
    pub targets:      Vec<String>,
}

#[derive(Debug)]
pub struct HookAction {
    pub when: HookWhen,
    pub exec: String,
    pub depends: Vec<String>,
    pub description: Option<String>,
    pub needs_targets: bool,
}

#[derive(Debug)]
pub struct PacmanHook {
    pub name: String,
    pub trigger: HookTrigger,
    pub action: HookAction,
}

const IGNORED_HOOKS: &[&str] = &[
    "60-mkinitcpio-remove.hook",
    "70-dkms-install.hook",
    "70-dkms-upgrade.hook",
    "71-dkms-remove.hook",
    "90-mkinitcpio-install.hook",
    "35-systemd-udev-reload.hook",
];

pub fn parse_hook_file(path: &Path) -> anyhow::Result<PacmanHook> {
    let content = fs::read_to_string(path)?;
    let name = path.file_name().unwrap().to_string_lossy().to_string();

    let mut trigger_type = None;
    let mut operations: Vec<HookOperation> = Vec::new();
    let mut targets: Vec<String> = Vec::new();
    let mut when = None;
    let mut exec = None;
    let mut depends: Vec<String> = Vec::new();
    let mut description = None;
    let mut needs_targets = false;

    let mut section = String::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len()-1].to_string();
            continue;
        }

        let (key, value) = line.split_once('=')
            .map(|(k, v)| (k.trim(), v.trim()))
            .ok_or_else(|| anyhow::anyhow!("Invalid hook line: {}", line))?;

        match (section.as_str(), key) {
            ("Trigger", "Type") => {
                trigger_type = Some(match value {
                    "File"    => HookTriggerType::File,
                    "Package" => HookTriggerType::Package,
                    "Path"    => HookTriggerType::Path,
                    _ => anyhow::bail!("Unknown trigger type: {}", value),
                });
            }
            ("Trigger", "Operation") => {
                let op = match value {
                    "Install" => HookOperation::Install,
                    "Upgrade" => HookOperation::Upgrade,
                    "Remove"  => HookOperation::Remove,
                    _ => anyhow::bail!("Unknown operation: {}", value),
                };
                if !operations.contains(&op) {
                    operations.push(op);
                }
            }
            ("Trigger", "Target") => targets.push(value.to_string()),
            ("Action", "When") => {
                when = Some(match value {
                    "PreTransaction"  => HookWhen::PreTransaction,
                    "PostTransaction" => HookWhen::PostTransaction,
                    _ => anyhow::bail!("Unknown When: {}", value),
                });
            }
            ("Action", "Exec") => exec = Some(value.to_string()),
            ("Action", "Depends") => depends.push(value.to_string()),
            ("Action", "Description") => description = Some(value.to_string()),
            ("Action", "NeedsTargets") => needs_targets = value.eq_ignore_ascii_case("true"),
            _ => {}
        }
    }

    if operations.is_empty() {
        anyhow::bail!("Missing Trigger.Operation in hook: {}", name);
    }

    Ok(PacmanHook {
        name,
        trigger: HookTrigger {
            trigger_type: trigger_type.ok_or_else(|| anyhow::anyhow!("Missing Trigger.Type"))?,
            operations,
            targets,
        },
        action: HookAction {
            when:         when.ok_or_else(|| anyhow::anyhow!("Missing Action.When"))?,
            exec:         exec.ok_or_else(|| anyhow::anyhow!("Missing Action.Exec"))?,
            depends,
            description,
            needs_targets,
        },
    })
}

pub fn load_hooks(dest: &str) -> anyhow::Result<Vec<PacmanHook>> {
    let hook_dirs = [
        format!("{}/usr/share/libalpm/hooks", dest),
        format!("{}/etc/pacman.d/hooks", dest),
    ];

    let mut hooks = Vec::new();

    for dir in &hook_dirs {
        let path = Path::new(dir);
        if !path.exists() {
            continue;
        }

        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let path = entry.path();

            if path.extension().and_then(|e| e.to_str()) != Some("hook") {
                continue;
            }

            let filename = path.file_name()
                .unwrap()
                .to_string_lossy()
                .to_string();

            if IGNORED_HOOKS.contains(&filename.as_str()) {
                continue;
            }

            if let Ok(hook) = parse_hook_file(&path) {
                hooks.push(hook);
            }
        }
    }

    hooks.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(hooks)
}

/// Rozdziela listę targetów na wzorce pozytywne i negatywne (z prefiksem `!`).
/// Zwraca (positive_patterns, negative_patterns) – oba bez prefiksu `!`.
fn split_targets(targets: &[String]) -> (Vec<&str>, Vec<&str>) {
    let mut positive = Vec::new();
    let mut negative = Vec::new();

    for t in targets {
        if let Some(negated) = t.strip_prefix('!') {
            negative.push(negated);
        } else {
            positive.push(t.as_str());
        }
    }

    (positive, negative)
}

/// Sprawdza, czy dany element pasuje do zestawu wzorców.
///
/// Reguły zgodne z pacmanem:
/// 1. Element musi pasować do co najmniej jednego wzorca pozytywnego.
/// 2. Element nie może pasować do żadnego wzorca negatywnego (z prefiksem `!`).
///
/// Jeśli nie ma żadnych wzorców pozytywnych, element jest odrzucany.
fn matches_targets(targets: &[String], value: &str) -> bool {
    let (positive, negative) = split_targets(targets);

    if positive.is_empty() {
        return false;
    }

    let matched_positive = positive.iter().any(|pat| glob_match(pat, value));
    if !matched_positive {
        return false;
    }

    let excluded = negative.iter().any(|pat| glob_match(pat, value));
    !excluded
}

pub fn hook_matches(
    hook: &PacmanHook,
    active_operations: &[HookOperation],
    installed_packages: &[String],
    installed_files: &[String],
) -> Vec<String> {
    // Hook musi mieć przynajmniej jedną operację wspólną z aktualnie wykonywanymi.
    let operation_matches = hook.trigger.operations.iter()
        .any(|op| active_operations.contains(op));

    if !operation_matches {
        return vec![];
    }

    let mut matched = Vec::new();

    match hook.trigger.trigger_type {
        HookTriggerType::Package => {
            for pkg in installed_packages {
                if matches_targets(&hook.trigger.targets, pkg) {
                    matched.push(pkg.clone());
                }
            }
        }
        HookTriggerType::File | HookTriggerType::Path => {
            for file in installed_files {
                let file_rel = file.trim_start_matches('/');

                // Wzorce w hooku mogą, ale nie muszą, zaczynać się od `/`.
                // Normalizujemy oba do postaci bez wiodącego ukośnika.
                let normalized_targets: Vec<String> = hook.trigger.targets
                    .iter()
                    .map(|t| {
                        if let Some(negated) = t.strip_prefix('!') {
                            format!("!{}", negated.trim_start_matches('/'))
                        } else {
                            t.trim_start_matches('/').to_string()
                        }
                    })
                    .collect();

                if matches_targets(&normalized_targets, file_rel) {
                    matched.push(file.clone());
                }
            }
        }
    }

    matched.dedup();
    matched
}

pub fn glob_match(pattern: &str, s: &str) -> bool {
    glob::Pattern::new(pattern)
        .map(|p| p.matches(s))
        .unwrap_or(false)
}