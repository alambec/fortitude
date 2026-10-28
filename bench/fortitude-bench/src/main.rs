//! Load generator used to compare Fortitude against other DICOMweb servers (Orthanc).
//!
//! The workflow is: `generate` a dataset once from seed files, `stow` it into every server
//! under test, then run the `read` scenarios against each of them with identical parameters.

mod generate;
mod http;
mod manifest;
mod read;
mod report;
mod rng;
mod stow;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(about = "Benchmark tool for DICOMweb servers")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Build a synthetic dataset by copying seed files with fresh UIDs and demographics.
    Generate(generate::Args),
    /// Upload a generated dataset with STOW-RS and measure ingest throughput.
    Stow(stow::Args),
    /// Run a read scenario (QIDO-RS, WADO-RS, WADO-URI) for a fixed duration.
    Read(read::Args),
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Generate(args) => generate::run(&args),
        Command::Stow(args) => runtime()?.block_on(stow::run(args)),
        Command::Read(args) => runtime()?.block_on(read::run(args)),
    }
}

fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
}
