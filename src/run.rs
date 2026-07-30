//! The run phase: execute one shard's share, and record what each unit took.
//!
//! Two axes, independent of each other.
//!
//! **Where membership comes from.** Without `--enumerate` the shard takes the slice
//! the plan computed, so enumeration had to happen before the fan-out. With
//! `--enumerate` the shard derives it from its own hydrated build; assignment is a
//! deterministic function of (universe, durations, shard count), so the shards agree
//! without coordinating and the plan may be built beside the build rather than in
//! front of it. A unit the plan never predicted is then assigned by construction
//! and reported as drift instead of being silently skipped.
//!
//! **Where durations come from.** `Reported` runs the slice once and asks the recipe
//! to turn the runner's own report into `unit<TAB>seconds`. `Measured` invokes the
//! test hook once per unit and times it from the outside, which needs no knowledge
//! of the runner at all and costs one process launch per unit.

use crate::hook;
use crate::plan::{self, Plan};
use crate::recipe::{Recipe, TimingMode};
use crate::timings::{self, Record};
use anyhow::{bail, Context, Result};
use clap::Args;
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Args)]
pub struct RunArgs {
    /// Built-in recipe supplying the hooks. `shard-tests recipes` lists them.
    #[arg(long)]
    recipe: Option<String>,

    /// A JSON array of recipes to look `--recipe` up in, instead of the built-ins.
    #[arg(long)]
    recipe_file: Option<PathBuf>,

    #[arg(long, default_value = "shard-tests-plan.json")]
    plan: PathBuf,

    /// 1-based shard index, i.e. the matrix value produced by the plan phase.
    #[arg(long)]
    index: usize,

    /// Shell snippet executed with `SHARD_TESTS_UNITS` set to this shard's ids.
    /// Overrides the recipe's.
    #[arg(long)]
    run: Option<String>,

    /// Shell snippet printing one unit id per line, evaluated in this shard.
    ///
    /// Give this when the plan was made without a build: it makes real membership
    /// authoritative, so a unit the plan did not predict is assigned rather than
    /// skipped. Must be cheap — listing from a hydrated artifact qualifies,
    /// compiling the suite again does not.
    #[arg(long)]
    enumerate: Option<String>,

    /// Derive membership locally using the recipe's own enumerate hook.
    ///
    /// Opt-in rather than implied by `--recipe`: the static slice is the default,
    /// and deriving locally is the choice that goes with a plan built beside the
    /// build. Equivalent to passing the recipe's enumerate to `--enumerate`.
    #[arg(long)]
    derive: bool,

    /// Treat any difference between the predicted and real universe as an error.
    #[arg(long)]
    fail_on_drift: bool,

    /// Append a timing record per unit here, as JSONL.
    #[arg(long)]
    timings_out: Option<PathBuf>,

    /// Name recorded with each timing, and the one `plan --runner` selects on.
    #[arg(long, default_value = "")]
    runner: String,

    /// Passed to the hooks as `SHARD_TESTS_EXTRA`.
    ///
    /// The injection point that keeps a recipe from being a wall: coverage flags and
    /// the like go here rather than into a rewritten recipe.
    ///
    /// `allow_hyphen_values` because the value almost always starts with `--`.
    #[arg(long, default_value = "", allow_hyphen_values = true)]
    extra: String,

    /// Where a `Reported` recipe writes its report, exposed as `SHARD_TESTS_REPORT`.
    #[arg(long, default_value = "shard-tests-report.json")]
    report_file: PathBuf,

    /// String joining the unit ids in `SHARD_TESTS_UNITS`. Overrides the recipe's.
    #[arg(long)]
    separator: Option<String>,

    /// Also write the unit ids, one per line, to this path.
    #[arg(long)]
    units_file: Option<PathBuf>,
}

pub fn main(args: RunArgs) -> Result<()> {
    let recipe = match &args.recipe {
        Some(name) => {
            let r = Recipe::find(name, args.recipe_file.as_deref())?;
            r.announce();
            Some(r)
        }
        None => None,
    };

    let test = args
        .run
        .clone()
        .or_else(|| recipe.as_ref().map(|r| r.test.clone()))
        .context("no test command: pass --recipe or --run")?;
    let separator = args
        .separator
        .clone()
        .or_else(|| recipe.as_ref().map(|r| r.separator.clone()))
        .unwrap_or_else(|| "\n".to_string());
    let enumerate = match (&args.enumerate, args.derive) {
        (Some(hook), _) => Some(hook.clone()),
        (None, true) => Some(
            recipe
                .as_ref()
                .map(|r| r.enumerate.clone())
                .context("--derive needs --recipe, or pass --enumerate directly")?,
        ),
        (None, false) => None,
    };
    let mode = recipe
        .as_ref()
        .map(|r| r.timing_mode)
        .unwrap_or(TimingMode::Reported);

    let plan = Plan::load(&args.plan)?;
    if args.index == 0 || args.index > plan.shards {
        bail!(
            "--index {} is outside the planned range 1..={}",
            args.index,
            plan.shards
        );
    }

    let (units, predicted) = match &enumerate {
        None => static_slice(&plan, args.index),
        Some(hook) => derived_slice(&plan, args.index, hook, args.fail_on_drift)?,
    };

    // An empty shard is legitimate once the count comes from measured work rather
    // than from a number in YAML, so it succeeds rather than fails.
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
        predicted
    );

    let index = args.index.to_string();
    let total = plan.shards.to_string();
    let report_file = args.report_file.display().to_string();
    let base: Vec<(&str, &str)> = vec![
        ("SHARD_TESTS_INDEX", index.as_str()),
        ("SHARD_TESTS_TOTAL", total.as_str()),
        ("SHARD_TESTS_EXTRA", args.extra.as_str()),
        ("SHARD_TESTS_REPORT", report_file.as_str()),
    ];

    let records = match mode {
        TimingMode::Measured => run_measured(&test, &units, &base, &args.runner)?,
        TimingMode::Reported => run_reported(
            &test,
            &units,
            &separator,
            &base,
            recipe.as_ref(),
            &args.runner,
        )?,
    };

    if let Some(path) = &args.timings_out {
        timings::append(path, &records)?;
        eprintln!(
            "shard-tests: recorded {} timing(s) to {}",
            records.len(),
            path.display()
        );
    }

    let failed: Vec<&Record> = records.iter().filter(|r| r.outcome == "fail").collect();
    if !failed.is_empty() {
        for r in &failed {
            eprintln!("  failed: {}", r.unit);
        }
        bail!("{} of {} unit(s) failed", failed.len(), records.len());
    }
    Ok(())
}

/// One invocation per unit, timed from the outside.
///
/// Every unit is attempted even after one fails: stopping early would leave the
/// rest of the shard unmeasured and the rest of the failures unreported, and a
/// partial picture is what makes a red shard expensive to diagnose.
fn run_measured(
    test: &str,
    units: &[String],
    base: &[(&str, &str)],
    runner: &str,
) -> Result<Vec<Record>> {
    let mut records = Vec::with_capacity(units.len());
    for (n, unit) in units.iter().enumerate() {
        let mut env = base.to_vec();
        env.push(("SHARD_TESTS_UNITS", unit.as_str()));
        eprintln!("shard-tests: [{}/{}] {unit}", n + 1, units.len());

        let started = Instant::now();
        let outcome = match hook::status("test", test, &env) {
            Ok(()) => "pass",
            Err(_) => "fail",
        };
        let seconds = started.elapsed().as_secs_f64();
        records.push(Record::new(unit.clone(), seconds, outcome, runner));
    }
    Ok(records)
}

/// One invocation for the whole slice, then the recipe turns its report into
/// `unit<TAB>seconds`.
fn run_reported(
    test: &str,
    units: &[String],
    separator: &str,
    base: &[(&str, &str)],
    recipe: Option<&Recipe>,
    runner: &str,
) -> Result<Vec<Record>> {
    let joined = units.join(separator);
    let mut env = base.to_vec();
    env.push(("SHARD_TESTS_UNITS", joined.as_str()));

    let outcome = hook::status("test", test, &env);

    let report = match recipe.and_then(|r| r.report.clone()) {
        Some(hook) => hook,
        None => {
            // No report hook, so nothing can be recorded — but the test result still
            // has to be honoured.
            outcome?;
            return Ok(Vec::new());
        }
    };

    // Read the report even when the tests failed: the durations of what did run are
    // still the best estimate available, and discarding them would make a red run
    // silently degrade the next plan's balance.
    let parsed = match hook::capture("report", &report, &env) {
        Ok(text) => parse_report(&text, runner),
        Err(e) => {
            eprintln!("shard-tests: could not read the timing report ({e})");
            Vec::new()
        }
    };
    outcome?;
    Ok(parsed)
}

/// Parses `unit<TAB>seconds`, ignoring lines that are not that.
fn parse_report(text: &str, runner: &str) -> Vec<Record> {
    let mut records = Vec::new();
    for line in text.lines() {
        let mut parts = line.splitn(2, '\t');
        let (Some(unit), Some(seconds)) = (parts.next(), parts.next()) else {
            continue;
        };
        let unit = unit.trim();
        if unit.is_empty() {
            continue;
        }
        match seconds.trim().parse::<f64>() {
            Ok(seconds) => records.push(Record::new(unit.to_string(), seconds, "pass", runner)),
            Err(_) => eprintln!("shard-tests: report line has a non-numeric duration: {line}"),
        }
    }
    records
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
    let real = plan::parse_units(&hook::capture("enumerate", hook_script, &[])?)?;

    // Only genuinely measured entries become timings again: a unit the plan fell
    // back to `default_seconds` for was never measured, and reading it back as a
    // measurement would make the durations disagree with what a fresh plan would
    // compute from the same inputs.
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
