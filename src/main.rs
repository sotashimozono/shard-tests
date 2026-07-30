//! `shard-tests` — split any test suite across CI jobs.
//!
//! The binary knows nothing about any test framework. A recipe supplies three
//! shell snippets — `prepare`, `enumerate`, `run` — and the unit of sharding is
//! whatever `enumerate` prints. That is what lets a file-per-unit suite, a
//! function-per-unit suite and a package-per-unit suite share one implementation.

mod claim;
mod hook;
mod plan;
mod run;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "shard-tests", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Enumerate the suite and write a plan: shard count, claim order, assignment.
    Plan(plan::PlanArgs),
    /// Run one shard's share of a plan.
    Run(run::RunArgs),
    /// Take the next unit from the run's queue. Not implemented yet.
    Claim(claim::ClaimArgs),
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Plan(args) => plan::main(args),
        Command::Run(args) => run::main(args),
        Command::Claim(args) => claim::main(args),
    }
}
