//! Recorded per-unit durations, as append-only JSONL.
//!
//! JSONL rather than a `{unit: seconds}` object for three reasons. Appending is
//! conflict-free, so N shards each write their own lines and the store is their
//! concatenation — there is no merge step to get wrong. Provenance becomes a field
//! instead of a filename, so one store holds every runner and the reader selects
//! (a Windows job can be twice a Linux one, and balancing across both would be
//! wrong). And keeping the observations means smoothing happens at read time, so
//! the policy can change later without the raw numbers having been destroyed.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Record {
    pub unit: String,
    pub seconds: f64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub runner: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub outcome: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sha: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub run: String,
}

impl Record {
    /// Stamps provenance from the CI environment, so a store gathered from several
    /// jobs stays readable afterwards.
    pub fn new(unit: String, seconds: f64, outcome: &str, runner: &str) -> Self {
        let env = |k: &str| std::env::var(k).unwrap_or_default();
        Record {
            unit,
            seconds,
            runner: runner.to_string(),
            outcome: outcome.to_string(),
            sha: env("GITHUB_SHA"),
            run: env("GITHUB_RUN_ID"),
        }
    }
}

/// Reads JSONL, reporting the line that failed rather than the whole file.
pub fn load(path: &Path) -> Result<Vec<Record>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("could not read timings {}", path.display()))?;
    let mut records = Vec::new();
    for (n, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record: Record = serde_json::from_str(line)
            .with_context(|| format!("{}:{} is not a timing record", path.display(), n + 1))?;
        records.push(record);
    }
    Ok(records)
}

/// Appends records, creating the file if absent.
pub fn append(path: &Path, records: &[Record]) -> Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .with_context(|| format!("could not open {} for appending", path.display()))?;
    for record in records {
        writeln!(file, "{}", serde_json::to_string(record)?)
            .with_context(|| format!("could not append to {}", path.display()))?;
    }
    Ok(())
}

/// Collapses observations into one duration per unit.
///
/// Filters to a runner when asked, keeps the last `keep` observations of each unit
/// — file order is append order, so the last are the most recent — and takes their
/// median. Median rather than mean because a single job that hit a cold cache or a
/// noisy neighbour should not move the estimate.
pub fn reduce(records: &[Record], runner: Option<&str>, keep: usize) -> HashMap<String, f64> {
    let mut by_unit: HashMap<&str, Vec<f64>> = HashMap::new();
    for record in records {
        if let Some(want) = runner {
            if record.runner != want {
                continue;
            }
        }
        by_unit
            .entry(&record.unit)
            .or_default()
            .push(record.seconds);
    }

    by_unit
        .into_iter()
        .map(|(unit, mut seen)| {
            if keep > 0 && seen.len() > keep {
                seen.drain(..seen.len() - keep);
            }
            seen.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let mid = seen.len() / 2;
            let median = if seen.len() % 2 == 0 {
                (seen[mid - 1] + seen[mid]) / 2.0
            } else {
                seen[mid]
            };
            (unit.to_string(), median)
        })
        .collect()
}

/// Trims the store to the last `keep` observations of each unit and runner.
///
/// Also drops units absent from `universe` when one is given: without that the
/// store accumulates deleted tests forever, and a stale unit still counts toward
/// the predicted total that decides the shard count.
pub fn compact(records: &[Record], keep: usize, universe: Option<&[String]>) -> Vec<Record> {
    let alive: Option<std::collections::HashSet<&str>> =
        universe.map(|u| u.iter().map(String::as_str).collect());

    // Walk backwards so "the last `keep`" is what survives, then restore order.
    let mut seen: HashMap<(&str, &str), usize> = HashMap::new();
    let mut kept: Vec<&Record> = Vec::new();
    for record in records.iter().rev() {
        if let Some(alive) = &alive {
            if !alive.contains(record.unit.as_str()) {
                continue;
            }
        }
        let key = (record.unit.as_str(), record.runner.as_str());
        let count = seen.entry(key).or_default();
        if keep == 0 || *count < keep {
            *count += 1;
            kept.push(record);
        }
    }
    kept.reverse();
    kept.into_iter().cloned().collect()
}

/// Reads either JSONL or the flat `{unit: seconds}` object that earlier versions
/// wrote, so an existing store keeps working.
pub fn load_any(path: &Path, runner: Option<&str>, keep: usize) -> Result<HashMap<String, f64>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("could not read timings {}", path.display()))?;
    if let Ok(flat) = serde_json::from_str::<HashMap<String, f64>>(&raw) {
        return Ok(flat);
    }
    let records = load(path)?;
    if records.is_empty() {
        bail!(
            "{} holds no timing records — it is neither JSONL nor a unit-to-seconds object",
            path.display()
        );
    }
    let reduced = reduce(&records, runner, keep);
    if reduced.is_empty() {
        if let Some(runner) = runner {
            bail!(
                "{} has {} record(s) but none for runner {runner} — check the --runner name",
                path.display(),
                records.len()
            );
        }
    }
    Ok(reduced)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(unit: &str, seconds: f64, runner: &str) -> Record {
        Record {
            unit: unit.into(),
            seconds,
            runner: runner.into(),
            outcome: "pass".into(),
            sha: String::new(),
            run: String::new(),
        }
    }

    #[test]
    fn reduce_takes_the_median_of_the_most_recent_observations() {
        // Oldest first. With keep = 3 the leading 99.0 must not survive to skew it.
        let records = vec![
            rec("a", 99.0, "linux"),
            rec("a", 1.0, "linux"),
            rec("a", 2.0, "linux"),
            rec("a", 3.0, "linux"),
        ];
        let out = reduce(&records, Some("linux"), 3);
        assert_eq!(out["a"], 2.0);
    }

    #[test]
    fn reduce_selects_by_runner() {
        // A Windows job can be twice a Linux one; mixing them balances neither.
        let records = vec![rec("a", 1.0, "linux"), rec("a", 10.0, "windows")];
        assert_eq!(reduce(&records, Some("linux"), 5)["a"], 1.0);
        assert_eq!(reduce(&records, Some("windows"), 5)["a"], 10.0);
        assert_eq!(reduce(&records, None, 5)["a"], 5.5, "unfiltered takes both");
    }

    #[test]
    fn compact_keeps_the_last_n_per_unit_and_runner() {
        let records = vec![
            rec("a", 1.0, "linux"),
            rec("a", 2.0, "linux"),
            rec("a", 3.0, "linux"),
            rec("a", 9.0, "windows"),
        ];
        let out = compact(&records, 2, None);
        let linux: Vec<f64> = out
            .iter()
            .filter(|r| r.runner == "linux")
            .map(|r| r.seconds)
            .collect();
        assert_eq!(linux, vec![2.0, 3.0], "oldest dropped, order preserved");
        assert_eq!(out.iter().filter(|r| r.runner == "windows").count(), 1);
    }

    #[test]
    fn compact_drops_units_no_longer_in_the_universe() {
        // A deleted test would otherwise keep counting toward the predicted total
        // that decides the shard count.
        let records = vec![rec("alive", 1.0, "linux"), rec("deleted", 50.0, "linux")];
        let universe = vec!["alive".to_string()];
        let out = compact(&records, 5, Some(&universe));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].unit, "alive");
    }
}
