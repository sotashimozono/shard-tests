//! The run phase: execute one shard's share.
//!
//! Two modes, and the difference is where *membership* comes from.
//!
//! Without `--enumerate` the shard takes the slice the plan already computed. The
//! plan is then authoritative about which units exist, which means enumeration had
//! to happen before the fan-out — serially in front of every shard.
//!
//! With `--enumerate` the shard derives membership itself, from its own hydrated
//! build. Assignment is a deterministic function of (universe, durations, shard
//! count), so every shard reaches the same partition without talking to any other,
//! and the plan is demoted to what it can supply without a build: the shard count
//! and the timings. Planning then runs *beside* the build instead of in front of
//! it. The plan's universe becomes a prediction, and a unit the build has that the
//! prediction lacked is still assigned and still runs — it is reported as drift,
//! not silently dropped.

use crate::hook;
use crate::plan::{self, Plan};
use anyhow::{bail, Result};
use clap::Args;
use std::collections::HashSet;
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

    /// Shell snippet printing one unit id per line, evaluated in this shard.
    ///
    /// Give this when the plan was made without a build — the universe it holds is
    /// then a prediction, and this is what makes real membership authoritative.
    /// Must be cheap: it runs once per shard. Listing from a hydrated build
    /// artifact qualifies; compiling the suite again does not.
    #[arg(long)]
    enumerate: Option<String>,

    /// Treat any difference between the predicted and the real universe as an error.
    #[arg(long)]
    fail_on_drift: bool,

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

    let (units, predicted_seconds) = match &args.enumerate {
        None => static_slice(&plan, args.index),
        Some(hook) => derived_slice(&plan, args.index, hook, args.fail_on_drift)?,
    };

    // An empty shard is a legitimate outcome once the shard count is chosen from
    // measured work rather than fixed in YAML, so it succeeds rather than fails.
    if units.is_empty() {
        eprintln!(
            "shard-tests: shard {}/{} has no units, nothing to run",
            args.index, plan.shards
        );
        return Ok(());
    }

    if let Some(path) = &args.units_file {
        std::fs::write(path, format!("{}\n", units.join("\n")))?;
    }

    eprintln!(
        "shard-tests: shard {}/{}, {} unit(s), {:.1}s predicted",
        args.index,
        plan.shards,
        units.len(),
        predicted_seconds
    );

    let joined = units.join(&args.separator);
    hook::status(
        &args.run,
        &[
            ("SHARD_TESTS_UNITS", joined.as_str()),
            ("SHARD_TESTS_INDEX", &args.index.to_string()),
            ("SHARD_TESTS_TOTAL", &plan.shards.to_string()),
        ],
    )
}

/// The slice the plan computed, for suites whose enumeration happened up front.
fn static_slice(plan: &Plan, index: usize) -> (Vec<String>, f64) {
    let mine = &plan.assignment[index - 1];
    let ids = mine.iter().map(|&i| plan.units[i].id.clone()).collect();
    let seconds = mine.iter().map(|&i| plan.units[i].seconds).sum();
    (ids, seconds)
}

/// Membership from this shard's own build, balanced with the plan's timings.
fn derived_slice(
    plan: &Plan,
    index: usize,
    hook_script: &str,
    fail_on_drift: bool,
) -> Result<(Vec<String>, f64)> {
    eprintln!("shard-tests: enumerate (membership from this shard's build)");
    let real = plan::parse_units(&hook::capture(hook_script, &[])?)?;

    // Only genuinely measured entries become timings again: a unit the plan fell
    // back to `default_seconds` for was never measured, and re-reading it as a
    // measurement would make the durations disagree with what a fresh plan would
    // compute for the same inputs.
    let timings = plan
        .units
        .iter()
        .filter(|u| u.measured)
        .map(|u| (u.id.clone(), u.seconds))
        .collect();

    let units = plan::build_units(&real, &timings, plan.default_seconds);
    let order = plan::lpt_order(&units);
    let assignment = plan::assign(&units, &order, plan.shards);

    let mine = &assignment[index - 1];
    let ids: Vec<String> = mine.iter().map(|&i| units[i].id.clone()).collect();
    let seconds: f64 = mine.iter().map(|&i| units[i].seconds).sum();

    report_drift(plan, &real, fail_on_drift)?;
    Ok((ids, seconds))
}

/// Compares the predicted universe against the real one.
///
/// Added units are already assigned by the time this runs — membership came from
/// the real universe — so this reports rather than repairs. It still matters: the
/// shard count was chosen from the predicted total, so large drift means the
/// balance is stale even though nothing was lost.
fn report_drift(plan: &Plan, real: &[String], fail_on_drift: bool) -> Result<()> {
    let predicted: HashSet<&str> = plan.units.iter().map(|u| u.id.as_str()).collect();
    let actual: HashSet<&str> = real.iter().map(String::as_str).collect();

    let mut added: Vec<&str> = actual.difference(&predicted).copied().collect();
    let mut gone: Vec<&str> = predicted.difference(&actual).copied().collect();
    added.sort_unstable();
    gone.sort_unstable();

    if added.is_empty() && gone.is_empty() {
        return Ok(());
    }

    eprintln!(
        "shard-tests: drift — {} unit(s) the plan did not predict, {} it predicted that are gone",
        added.len(),
        gone.len()
    );
    for id in added.iter().take(10) {
        eprintln!("  + {id}");
    }
    for id in gone.iter().take(10) {
        eprintln!("  - {id}");
    }
    if added.len() > 10 || gone.len() > 10 {
        eprintln!("  … truncated");
    }

    if fail_on_drift {
        bail!(
            "the real universe differs from the predicted one by {} unit(s) and --fail-on-drift is set",
            added.len() + gone.len()
        );
    }
    eprintln!(
        "shard-tests: the added units are assigned and will run; the shard count came from the \
         predicted total, so balance may be stale. Refresh the timings to settle it."
    );
    Ok(())
}
