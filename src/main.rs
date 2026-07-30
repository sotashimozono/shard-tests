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

use anyhow::{bail, Context, Result};
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
    /// Run the recipe's build once, and report what the shards must hydrate.
    Build(BuildArgs),
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
pub struct BuildArgs {
    /// Built-in recipe supplying the build command.
    #[arg(long)]
    recipe: Option<String>,

    /// A JSON array of recipes to look `--recipe` up in, instead of the built-ins.
    #[arg(long)]
    recipe_file: Option<PathBuf>,

    /// Build command, overriding the recipe's.
    #[arg(long)]
    build: Option<String>,

    /// Passed to the hook as `SHARD_TESTS_EXTRA`.
    #[arg(long, default_value = "", allow_hyphen_values = true)]
    extra: String,
}

/// Runs the build phase alone, so it can be a job of its own and the plan can run
/// beside it. Publishes the recipe's `transfers` as step outputs, so the caller
/// uploads exactly what the shards need to hydrate without repeating the list.
fn build(args: BuildArgs) -> Result<()> {
    let recipe = match &args.recipe {
        Some(name) => {
            let r = recipe::Recipe::find(name, args.recipe_file.as_deref())?;
            r.announce();
            Some(r)
        }
        None => None,
    };

    let command = args
        .build
        .clone()
        .or_else(|| recipe.as_ref().and_then(|r| r.build.clone()));

    match &command {
        Some(command) => {
            eprintln!("shard-tests: build");
            hook::status(
                "build",
                command,
                &[("SHARD_TESTS_EXTRA", args.extra.as_str())],
            )?;
        }
        None => eprintln!(
            "shard-tests: this recipe has no build phase — every shard prepares its own \
             environment instead, so there is nothing to hand over"
        ),
    }

    let transfers: Vec<String> = recipe.map(|r| r.transfers).unwrap_or_default();
    for path in &transfers {
        if !std::path::Path::new(path).exists() {
            bail!(
                "the build finished but {path} is missing, and the shards are meant to hydrate \
                 it — check the build command actually produced it"
            );
        }
    }
    emit_transfers(&transfers)
}

/// Publishes `transfers` and `has-transfers` as step outputs.
fn emit_transfers(transfers: &[String]) -> Result<()> {
    let block = format!(
        "transfers<<SHARD_TESTS_EOF\n{}\nSHARD_TESTS_EOF\nhas-transfers={}\n",
        transfers.join("\n"),
        !transfers.is_empty()
    );
    match std::env::var_os("GITHUB_OUTPUT") {
        Some(path) => {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&path)
                .context("could not open $GITHUB_OUTPUT")?;
            file.write_all(block.as_bytes())
                .context("could not write to $GITHUB_OUTPUT")?;
        }
        None => print!("{block}"),
    }
    Ok(())
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
        Command::Build(args) => build(args),
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
