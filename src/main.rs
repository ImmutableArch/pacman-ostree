mod package_manager;
mod package_solver;
mod compose;
mod composepost;
mod bubblewrap;
mod initramfs;
mod container;
mod fsutil;


use package_solver::{AlpmPool, AlpmPackage, AlpmDep, AlpmProvide};
use package_manager::{AlpmRepository, PackageManager};
mod package_installer;
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "pacman-ostree")]
#[command(about = "Arch Linux OSTree builder", long_about = None)]
struct Args {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Build an OSTree image
    Compose(compose::ComposeImageOpts),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    
    let args = Args::parse();

    match args.command {
        Commands::Compose(opts) => {
            compose::compose_image(opts).await?;
        }
    }
    Ok(())
}