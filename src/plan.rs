//! The plan phase: enumerate the canonical unit universe, choose a shard count,
//! and emit both a claim order and a static assignment.
//!
//! Everything downstream keys off the plan, so it is produced exactly once per
//! workflow run. Enumerating independently in every shard would be cheaper in
//! wall clock but would make a divergence between shards invisible — a unit that
//! silently fails to appear in one shard is indistinguishable from a unit that
//! does not exist. One enumeration means completeness is checkable.

use crate::hook;
use anyhow::{bail, Context, Result};
use clap::Args;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Args)]
pub struct PlanArgs {
    /// Shell snippet run once before enumeration: build, archive, codegen.
    ///
    /// Compiled languages need this — you cannot list test functions without
    /// compiling them, and the artifact it produces is what shards reuse instead
    /// of each building their own copy.
    #[arg(long)]
    prepare: Option<String>,

    /// Shell snippet printing one unit id per line on stdout.
    ///
    /// The granularity is entirely the recipe's choice: a file, a test function,
    /// a package, a test set. Ids must be stable across runs, because recorded
    /// timings are keyed on them.
    #[arg(long)]
    enumerate: String,

    /// JSON object mapping unit id to seconds, recorded by a previous run.
    #[arg(long)]
    timings: Option<PathBuf>,

    /// Seconds of test work to aim for per shard, excluding fixed job overhead.
    #[arg(long, default_value_t = 60.0)]
    target_seconds: f64,

    /// Duration assumed for a unit that has no recorded timing.
    #[arg(long, default_value_t = 1.0)]
    default_seconds: f64,

    #[arg(long, default_value_t = 1)]
    min_shards: usize,

    #[arg(long, default_value_t = 16)]
    max_shards: usize,

    /// Where to write the plan.
    #[arg(long, default_value = "shard-tests-plan.json")]
    output: PathBuf,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Unit {
    pub id: String,
    pub seconds: f64,
    /// False when `seconds` is the `--default-seconds` fallback rather than a
    /// measurement. Balance quality is only as good as this fraction.
    pub measured: bool,
}

#[derive(Serialize, Deserialize)]
pub struct Plan {
    /// The canonical universe, in enumeration order. A unit's index here is its
    /// identity everywhere else, which is what lets records from different shards
    /// merge and lets completeness be verified against a single list.
    pub units: Vec<Unit>,

    /// Unit indices in longest-processing-time-first order.
    ///
    /// Both paths consume this: the static assignment packs it greedily, and the
    /// claim queue hands it out in the same order. Large units first is what
    /// keeps the tail short either way.
    pub order: Vec<usize>,

    pub shards: usize,
    pub total_seconds: f64,
    /// Fraction of total time backed by a real measurement.
    pub measured_fraction: f64,

    /// Shard index to unit indices. Used directly when claiming is unavailable —
    /// notably on pull requests from forks, where the job has no write token.
    pub assignment: Vec<Vec<usize>>,
}

impl Plan {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("could not read plan {}", path.display()))?;
        serde_json::from_str(&raw)
            .with_context(|| format!("{} is not a shard-tests plan", path.display()))
    }
}

pub fn main(args: PlanArgs) -> Result<()> {
    if args.min_shards == 0 {
        bail!("--min-shards must be at least 1");
    }
    if args.max_shards < args.min_shards {
        bail!(
            "--max-shards ({}) is below --min-shards ({})",
            args.max_shards,
            args.min_shards
        );
    }

    if let Some(prepare) = &args.prepare {
        eprintln!("shard-tests: prepare");
        hook::status(prepare, &[])?;
    }

    eprintln!("shard-tests: enumerate");
    let ids = parse_units(&hook::capture(&args.enumerate, &[])?)?;

    let timings = match &args.timings {
        Some(path) => load_timings(path)?,
        None => Default::default(),
    };

    let units: Vec<Unit> = ids
        .into_iter()
        .map(|id| match timings.get(&id) {
            Some(&seconds) => Unit {
                id,
                seconds,
                measured: true,
            },
            None => Unit {
                id,
                seconds: args.default_seconds,
                measured: false,
            },
        })
        .collect();

    let total_seconds: f64 = units.iter().map(|u| u.seconds).sum();
    let measured_seconds: f64 = units.iter().filter(|u| u.measured).map(|u| u.seconds).sum();
    // `> 0.0` on both, not just the divisor: the identity for `f64::sum` is -0.0,
    // so a suite with no measured unit yields -0.0 and reports "-0% measured".
    let measured_fraction = if total_seconds > 0.0 && measured_seconds > 0.0 {
        measured_seconds / total_seconds
    } else {
        0.0
    };

    let order = lpt_order(&units);
    let shards = choose_shards(
        total_seconds,
        args.target_seconds,
        args.min_shards,
        args.max_shards,
    );
    let assignment = assign(&units, &order, shards);

    let plan = Plan {
        units,
        order,
        shards,
        total_seconds,
        measured_fraction,
        assignment,
    };

    let json = serde_json::to_string_pretty(&plan)?;
    std::fs::write(&args.output, &json)
        .with_context(|| format!("could not write {}", args.output.display()))?;

    eprintln!(
        "shard-tests: {} units, {:.1}s total ({:.0}% measured) -> {} shard(s)",
        plan.units.len(),
        plan.total_seconds,
        plan.measured_fraction * 100.0,
        plan.shards
    );
    for (index, shard) in plan.assignment.iter().enumerate() {
        let seconds: f64 = shard.iter().map(|&i| plan.units[i].seconds).sum();
        eprintln!(
            "  shard {}: {} unit(s), {:.1}s",
            index + 1,
            shard.len(),
            seconds
        );
    }

    // A unit is indivisible, so the longest one is a floor on wall clock that no
    // shard count can lower. Saying so is the difference between an actionable
    // report and a plan that merely looks balanced.
    if let Some(critical) = plan.units.iter().max_by(|a, b| {
        a.seconds
            .partial_cmp(&b.seconds)
            .unwrap_or(std::cmp::Ordering::Equal)
    }) {
        if critical.seconds > args.target_seconds {
            eprintln!(
                "shard-tests: warning: unit {} alone takes {:.1}s, above the {:.1}s target. \
                 More shards cannot help; split that unit or enumerate at a finer granularity.",
                critical.id, critical.seconds, args.target_seconds
            );
        }
    }

    if plan.measured_fraction < 1.0 {
        eprintln!(
            "shard-tests: note: {:.0}% of predicted time is measured; the rest assumes {:.1}s per unit",
            plan.measured_fraction * 100.0,
            args.default_seconds
        );
    }

    emit_outputs(&plan)
}

/// Parses one unit id per line, rejecting duplicates.
///
/// A duplicate id would break identity: two distinct units sharing a key means
/// timings collide and "did every unit run exactly once" stops being answerable.
/// Failing here is much cheaper than a silently mis-balanced suite.
fn parse_units(raw: &str) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    let mut seen = HashSet::new();
    for line in raw.lines() {
        let id = line.trim();
        if id.is_empty() {
            continue;
        }
        if !seen.insert(id) {
            bail!("the enumerate hook emitted a duplicate unit id: {id}");
        }
        ids.push(id.to_string());
    }
    if ids.is_empty() {
        bail!("the enumerate hook produced no units — check the recipe, an empty suite is never assumed");
    }
    Ok(ids)
}

fn load_timings(path: &Path) -> Result<std::collections::HashMap<String, f64>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("could not read timings {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| {
        format!(
            "{} is not a JSON object mapping unit id to seconds",
            path.display()
        )
    })
}

/// Longest-processing-time-first, ties broken by enumeration order so the result
/// is a deterministic function of the plan inputs.
fn lpt_order(units: &[Unit]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..units.len()).collect();
    order.sort_by(|&a, &b| {
        units[b]
            .seconds
            .partial_cmp(&units[a].seconds)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.cmp(&b))
    });
    order
}

/// The smallest shard count whose predicted per-shard work fits `target_seconds`.
///
/// Deliberately not "as many shards as allowed": every shard pays a fixed job
/// cost (runner allocation, checkout, dependency restore), so past the point
/// where test work stops dominating that cost, more shards only add overhead.
fn choose_shards(total_seconds: f64, target: f64, min: usize, max: usize) -> usize {
    let wanted = if target > 0.0 {
        (total_seconds / target).ceil().max(1.0) as usize
    } else {
        max
    };
    wanted.clamp(min, max)
}

/// Greedy LPT bin packing: each unit goes to the least loaded shard.
fn assign(units: &[Unit], order: &[usize], shards: usize) -> Vec<Vec<usize>> {
    let mut load = vec![0.0f64; shards];
    let mut assignment = vec![Vec::new(); shards];
    for &unit in order {
        let target = load
            .iter()
            .enumerate()
            .min_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(index, _)| index)
            .expect("shards >= 1");
        load[target] += units[unit].seconds;
        assignment[target].push(unit);
    }
    assignment
}

/// Publishes `shards` and `matrix` as step outputs so a downstream job can fan
/// out with `fromJSON`. GitHub Actions cannot build a dynamic matrix without a
/// prior job's output, which is why the plan phase is a job of its own.
fn emit_outputs(plan: &Plan) -> Result<()> {
    let matrix = serde_json::json!({
        "index": (1..=plan.shards).collect::<Vec<usize>>(),
    });
    let lines = format!(
        "shards={}\nmatrix={}\nunits={}\n",
        plan.shards,
        matrix,
        plan.units.len()
    );
    match std::env::var_os("GITHUB_OUTPUT") {
        Some(path) => {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&path)
                .with_context(|| "could not open $GITHUB_OUTPUT")?;
            file.write_all(lines.as_bytes())
                .context("could not write to $GITHUB_OUTPUT")?;
        }
        None => print!("{lines}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units_from(specs: &[(&str, f64)]) -> Vec<Unit> {
        specs
            .iter()
            .map(|&(id, seconds)| Unit {
                id: id.to_string(),
                seconds,
                measured: true,
            })
            .collect()
    }

    fn sample() -> Vec<Unit> {
        units_from(&[
            ("a", 30.0),
            ("b", 25.0),
            ("c", 10.0),
            ("d", 8.0),
            ("e", 2.0),
            ("f", 1.0),
            ("g", 1.0),
        ])
    }

    #[test]
    fn parse_units_keeps_enumeration_order_and_ignores_blank_lines() {
        let ids = parse_units("  b \n\n a\n\nc\n").unwrap();
        assert_eq!(ids, vec!["b", "a", "c"]);
    }

    #[test]
    fn parse_units_rejects_a_duplicate_id() {
        let err = parse_units("x\ny\nx\n").unwrap_err().to_string();
        assert!(err.contains("duplicate"), "unexpected message: {err}");
    }

    #[test]
    fn parse_units_rejects_an_empty_suite() {
        // An enumerate recipe that matches nothing is a broken recipe far more
        // often than it is a suite with no tests, and treating it as the latter
        // reports success while running nothing.
        assert!(parse_units("\n  \n").is_err());
    }

    #[test]
    fn lpt_order_is_descending_and_breaks_ties_by_enumeration_order() {
        let units = units_from(&[("a", 1.0), ("b", 5.0), ("c", 5.0), ("d", 2.0)]);
        assert_eq!(lpt_order(&units), vec![1, 2, 3, 0]);
    }

    #[test]
    fn choose_shards_takes_the_smallest_count_meeting_the_target() {
        assert_eq!(choose_shards(77.0, 20.0, 1, 16), 4);
        assert_eq!(choose_shards(77.0, 20.0, 1, 2), 2, "clamped by max");
        assert_eq!(choose_shards(5.0, 20.0, 3, 16), 3, "clamped by min");
        assert_eq!(choose_shards(0.0, 20.0, 1, 16), 1);
        assert_eq!(choose_shards(77.0, 0.0, 1, 8), 8, "no target means use max");
    }

    #[test]
    fn every_unit_is_assigned_exactly_once() {
        let units = sample();
        let order = lpt_order(&units);
        for shards in 1..=8 {
            let assignment = assign(&units, &order, shards);
            let mut seen: Vec<usize> = assignment.iter().flatten().copied().collect();
            seen.sort_unstable();
            assert_eq!(
                seen,
                (0..units.len()).collect::<Vec<_>>(),
                "shards={shards} lost or duplicated a unit"
            );
        }
    }

    #[test]
    fn assignment_meets_the_lpt_makespan_bound() {
        // Longest-processing-time-first is guaranteed within (4/3 - 1/3m) of the
        // optimal makespan, and the optimum is at least as large as both the
        // perfectly divided total and the longest indivisible unit. Comparing
        // against that arithmetic tests the packing against something other than
        // its own output.
        let units = sample();
        let order = lpt_order(&units);
        let total: f64 = units.iter().map(|u| u.seconds).sum();
        let longest = units.iter().map(|u| u.seconds).fold(0.0, f64::max);

        for shards in 1..=8 {
            let assignment = assign(&units, &order, shards);
            let makespan = assignment
                .iter()
                .map(|s| s.iter().map(|&i| units[i].seconds).sum::<f64>())
                .fold(0.0, f64::max);
            let optimum_at_least = (total / shards as f64).max(longest);
            let bound = (4.0 / 3.0 - 1.0 / (3.0 * shards as f64)) * optimum_at_least;
            assert!(
                makespan <= bound + 1e-9,
                "shards={shards}: makespan {makespan} exceeds the LPT bound {bound}"
            );
        }
    }

    #[test]
    fn a_shard_count_above_the_unit_count_leaves_empty_shards() {
        // Legitimate once the count is chosen from measured work, so it must be
        // representable rather than an error.
        let units = units_from(&[("a", 1.0), ("b", 1.0)]);
        let order = lpt_order(&units);
        let assignment = assign(&units, &order, 5);
        assert_eq!(assignment.len(), 5);
        assert_eq!(assignment.iter().filter(|s| s.is_empty()).count(), 3);
    }
}
