use clap::{Parser, Subcommand };
use std::error::Error;

mod compose;
mod pacman_manager;


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
}

fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Compose(opts) => {
            println!("Running compose with config: {:?}", opts.manifest);

            // Wczytanie YAML i dalsza logika
            let config = compose::yaml_parse(opts.manifest.as_str())?;
            compose::run(&config, &opts);

            // Tutaj dalsze kroki: instalacja pakietów, OSTree commit itd.
        }
    }

    Ok(())
}

