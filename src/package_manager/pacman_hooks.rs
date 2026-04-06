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

pub fn hook_matches(
    hook: &PacmanHook,
    installed_packages: &[String],
    installed_files: &[String],
) -> Vec<String> {
    if !hook.trigger.operations.contains(&HookOperation::Install) {
        return vec![];
    }

    let mut matched = Vec::new();

    for target_pattern in &hook.trigger.targets {
        match hook.trigger.trigger_type {
            HookTriggerType::Package => {
                for pkg in installed_packages {
                    if glob_match(target_pattern, pkg) {
                        matched.push(pkg.clone());
                    }
                }
            }
            HookTriggerType::File | HookTriggerType::Path => {
                for file in installed_files {
                    let file_rel = file.trim_start_matches('/');
                    let pattern_rel = target_pattern.trim_start_matches('/');

                    if glob_match(pattern_rel, file_rel) {
                        matched.push(file.clone());
                    }
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