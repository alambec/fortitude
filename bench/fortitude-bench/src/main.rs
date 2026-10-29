//! Load generator used to compare Fortitude against other DICOM servers (Orthanc).
//!
//! The workflow is: `generate` a dataset once from seed files, `stow` it into every server
//! under test, then run the `read` scenarios against each of them with identical parameters.
//! The `dimse-*` scenarios do the same over DIMSE.

mod dimse;
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
    /// Send a generated dataset with C-STORE and measure ingest throughput.
    DimseStore(dimse::StoreArgs),
    /// Run a C-FIND scenario for a fixed duration.
    DimseFind(dimse::FindArgs),
    /// C-MOVE every study of a dataset to a sink storage SCP run by the tool.
    DimseMove(dimse::MoveArgs),
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Generate(args) => generate::run(&args),
        Command::Stow(args) => runtime()?.block_on(stow::run(args)),
        Command::Read(args) => runtime()?.block_on(read::run(args)),
        Command::DimseStore(args) => runtime()?.block_on(dimse::store(args)),
        Command::DimseFind(args) => runtime()?.block_on(dimse::find(args)),
        Command::DimseMove(args) => runtime()?.block_on(dimse::r#move(args)),
    }
}

fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
}
