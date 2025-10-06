use clap::{Parser, Subcommand };
use std::error::Error;
use anyhow::{Context, Result};
use std::env;

mod compose;
mod pacman_manager;
mod container;


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
    Ostree {
        /// Pass-through arguments for ostree command
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    
}


#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>>
{
    let cli = Cli::parse();

    match cli.command {
        Commands::Compose(opts) => {
            println!("Running compose with config: {:?}", opts.manifest);

            // Wczytanie YAML i dalsza logika
            let config = compose::yaml_parse(opts.manifest.as_str())?;
            compose::run(&config, &opts).await;
        }
        Commands::Ostree { args } => {
            // Build argv for ostree-ext so argv[0] is the program name "ostree".
            // This is important: ostree_ext expects argv[0] to be the program name,
            // and argv[1].. to be the subcommand(s).
            let mut full_args = Vec::with_capacity(1 + args.len());
            full_args.push("ostree".to_string()); // program name
            full_args.extend(args.into_iter());

            // Now call ostree_ext with the constructed argv.
            // If ostree_ext::cli::run_from_iter returns a Result<()>, use `?` to propagate errors.
            ostree_ext::cli::run_from_iter(full_args).await?;
        }
    }

    Ok(())
}


