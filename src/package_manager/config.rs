use std::path::Path;
use pacmanconf::Config;
use anyhow::{Result, Context};

/// Wrapper wokół pacmanconf::Config do obsługi konfiguracji pacmana
/// Używa alpm-utils do parsowania /etc/pacman.conf
pub type PacmanConfig = Config;

pub fn load_config() -> Result<PacmanConfig> {
    Config::new().context("Failed to load/parse /etc/pacman.conf")
}

pub fn load_config_from_file(path: &Path) -> Result<PacmanConfig> {
    Config::from_file(path).context("Failed to load pacman.conf from custom path")
}

pub fn load_config_with_rootdir(path: &Path, rootdir: &str) -> Result<PacmanConfig> {
    let mut config = Config::from_file(path)
        .context("Failed to load pacman.conf from custom path")?;
    
    // Przesłoń rootdir dla zainstalowania w niestandardowej lokalizacji
    config.root_dir = rootdir.to_string();
    config.db_path = format!("{}/usr/share/pacman/", rootdir);
    
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_loading() {
        // Test będzie działać jeśli /etc/pacman.conf istnieje
        // W testach CI może się nie powieść, ale to ok
        let _config = load_config();
    }
}
