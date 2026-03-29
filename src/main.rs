use clap::{Parser, Subcommand};
use std::error::Error;
use anyhow::{Context, Result};
use std::env;
use camino::Utf8PathBuf;

mod compose;
mod pacman_manager;
mod container;
mod layering;
mod solver;

#[derive(Parser, Debug)]
#[command(author, version, about = "A program that connects pacman with ostree")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Compose Arch-based OSTree OCI image
    Compose(compose::ComposeImageOpts),

    /// Install packages into the current layered system
    Install {
        #[arg(required = true)]
        packages: Vec<String>,
    },
    /// Remove packages from the current layered systems
    Remove {
        #[arg(required = true)]
        packages: Vec<String>,
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    
    match cli.command {
        Commands::Compose(opts) => {
            println!("Running compose with config: {:?}", opts.manifest);
            let config = compose::yaml_parse(opts.manifest.as_str())?;
            compose::run(&config, &opts).await;
        }
        Commands::Install { packages } => {
            layering::install_packages(packages)?;
        }
        Commands::Remove { packages } => {
            layering::remove_packages(&packages)?;
        }
    }
    
    Ok(())
}