//! The run phase: execute one shard's share of the plan.

use crate::hook;
use crate::plan::Plan;
use anyhow::{bail, Result};
use clap::Args;
use std::path::PathBuf;

#[derive(Args)]
pub struct RunArgs {
    #[arg(long, default_value = "shard-tests-plan.json")]
    plan: PathBuf,

    /// 1-based shard index, i.e. the matrix value produced by the plan phase.
    #[arg(long)]
    index: usize,

    /// Shell snippet executed with `SHARD_TESTS_UNITS` set to this shard's ids.
    #[arg(long)]
    run: String,

    /// String joining the unit ids in `SHARD_TESTS_UNITS`.
    ///
    /// Newline suits runners that read a file or a list; a space or comma suits
    /// filter expressions built by string interpolation.
    #[arg(long, default_value = "\n")]
    separator: String,

    /// Also write the unit ids, one per line, to this path.
    #[arg(long)]
    units_file: Option<PathBuf>,
}

pub fn main(args: RunArgs) -> Result<()> {
    let plan = Plan::load(&args.plan)?;

    if args.index == 0 || args.index > plan.shards {
        bail!(
            "--index {} is outside the planned range 1..={}",
            args.index,
            plan.shards
        );
    }

    let units: Vec<&str> = plan.assignment[args.index - 1]
        .iter()
        .map(|&i| plan.units[i].id.as_str())
        .collect();

    // An empty shard is a legitimate outcome once shard count is chosen from
    // measured work rather than fixed in YAML, so it succeeds rather than fails.
    if units.is_empty() {
        eprintln!(
            "shard-tests: shard {}/{} has no units, nothing to run",
            args.index, plan.shards
        );
        return Ok(());
    }

    let joined = units.join(&args.separator);

    if let Some(path) = &args.units_file {
        std::fs::write(path, format!("{}\n", units.join("\n")))?;
    }

    let seconds: f64 = plan.assignment[args.index - 1]
        .iter()
        .map(|&i| plan.units[i].seconds)
        .sum();
    eprintln!(
        "shard-tests: shard {}/{}, {} unit(s), {:.1}s predicted",
        args.index,
        plan.shards,
        units.len(),
        seconds
    );

    hook::status(
        &args.run,
        &[
            ("SHARD_TESTS_UNITS", joined.as_str()),
            ("SHARD_TESTS_INDEX", &args.index.to_string()),
            ("SHARD_TESTS_TOTAL", &plan.shards.to_string()),
        ],
    )
}
