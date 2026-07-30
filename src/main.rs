//! `shard-tests` — split any test suite across CI jobs.
//!
//! The binary knows nothing about any test framework. Behaviour comes from a
//! recipe: three or four shell snippets plus the properties that decide how the CI
//! graph has to be assembled. The unit of sharding is whatever `enumerate` prints,
//! which is what lets a file-per-unit suite, a function-per-unit suite and a
//! binary-per-unit suite share one implementation.
//!
//! The phases, and which of them is a job of its own:
//!
//! ```text
//! build ─────────┐                     once, produces what the shards hydrate
//!                ├→ test ×N → collect  per shard
//! organize ──────┘                     once, beside the build when it can be
//!                    ↓
//!                 finalize             once, merges what the shards recorded
//! ```
//!
//! Whether `organize` can run beside `build` is a property of the recipe, not a
//! choice: it can when enumeration does not need the build.

mod claim;
mod hook;
mod plan;
mod recipe;
mod run;
mod timings;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "shard-tests", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Enumerate the suite and write a plan: shard count, claim order, assignment.
    #[command(alias = "organize")]
    Plan(plan::PlanArgs),
    /// Run one shard's share of a plan, recording what each unit took.
    #[command(alias = "test")]
    Run(run::RunArgs),
    /// Merge what the shards recorded into the timings store.
    Finalize(FinalizeArgs),
    /// Show the built-in recipes and where each was verified.
    Recipes,
    /// Take the next unit from the run's queue. Not implemented yet.
    Claim(claim::ClaimArgs),
}

#[derive(Args)]
pub struct FinalizeArgs {
    /// Per-shard JSONL files, as written by `run --timings-out`.
    inputs: Vec<PathBuf>,

    /// The store to merge into and rewrite. Read first when it exists.
    #[arg(long, default_value = "shard-tests-timings.jsonl")]
    store: PathBuf,

    /// Observations to keep per unit and runner. 0 keeps everything.
    #[arg(long, default_value_t = 5)]
    keep: usize,

    /// A file of current unit ids, one per line. Units absent from it are dropped,
    /// so a deleted test stops counting toward the total that sets the shard count.
    #[arg(long)]
    universe: Option<PathBuf>,
}

fn finalize(args: FinalizeArgs) -> Result<()> {
    let mut records = if args.store.exists() {
        timings::load(&args.store)?
    } else {
        Vec::new()
    };
    let before = records.len();

    for input in &args.inputs {
        records.extend(timings::load(input)?);
    }
    let gathered = records.len() - before;

    let universe = match &args.universe {
        Some(path) => {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("could not read {}", path.display()))?;
            Some(
                raw.lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>(),
            )
        }
        None => None,
    };

    let compacted = timings::compact(&records, args.keep, universe.as_deref());
    let dropped = records.len() - compacted.len();

    // Rewritten rather than appended: compaction is the whole point of this step,
    // and appending would leave the trimmed observations in place.
    if args.store.exists() {
        std::fs::remove_file(&args.store)?;
    }
    timings::append(&args.store, &compacted)?;

    eprintln!(
        "shard-tests: {before} stored + {gathered} new -> {} kept ({dropped} trimmed or retired)",
        compacted.len()
    );
    Ok(())
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Plan(args) => plan::main(args),
        Command::Run(args) => run::main(args),
        Command::Finalize(args) => finalize(args),
        Command::Recipes => {
            print!("{}", recipe::list());
            Ok(())
        }
        Command::Claim(args) => claim::main(args),
    }
}
