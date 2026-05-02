use clap::Parser;
use dbfs::{Cli, run};

fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
