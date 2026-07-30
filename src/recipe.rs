//! Built-in recipes.
//!
//! A recipe is data, not code: choosing `--recipe vitest` fills in every hook, the
//! separator, and the properties that decide how the CI graph must be assembled.
//! Adding an ecosystem is a row here, so nothing language-specific is ever
//! compiled in — and a row that turns out wrong is a one-line fix by whoever
//! actually uses that language.
//!
//! Only recipes that have been **executed against a real suite** ship here. The
//! `verified` field records where, and it is printed when the recipe is used. A
//! recipe nobody has run is worth less than no recipe at all: it reads as support
//! and behaves as a bug report.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum TimingMode {
    /// The runner reports per-unit durations; the `report` hook turns its output
    /// into `unit<TAB>seconds`. One invocation covers the whole slice.
    Reported,
    /// No per-unit report exists, so the `test` hook is invoked once per unit and
    /// timed from the outside. Needs no knowledge of the runner at all, and costs
    /// one process launch per unit.
    Measured,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Recipe {
    pub name: String,

    /// Runs once, producing artifacts the shards hydrate. Absent when every shard
    /// prepares its own environment instead.
    #[serde(default)]
    pub build: Option<String>,

    /// Prints one unit id per line.
    pub enumerate: String,

    /// Runs this shard's units, which arrive in `$SHARD_TESTS_UNITS`.
    pub test: String,

    /// Turns the runner's own report into `unit<TAB>seconds`. Required by
    /// `Reported`, unused by `Measured`.
    #[serde(default)]
    pub report: Option<String>,

    pub separator: String,

    /// Whether `enumerate` needs `build` to have run.
    ///
    /// This single property decides the shape of the CI graph. False means
    /// `organize` can run beside `build` from the very first run. True means the
    /// first run must enumerate after the build, and only later runs can go
    /// concurrent by predicting the universe from recorded timings.
    pub enumerate_needs_build: bool,

    /// Paths the build phase leaves behind for the shards to hydrate.
    #[serde(default)]
    pub transfers: Vec<String>,

    pub timing_mode: TimingMode,

    /// Where this recipe was actually executed. `None` means it was not.
    #[serde(default)]
    pub verified: Option<String>,

    #[serde(default)]
    pub notes: String,
}

impl Recipe {
    /// Looks up a recipe by name, preferring a local override file.
    pub fn find(name: &str, from: Option<&Path>) -> Result<Recipe> {
        let table = match from {
            Some(path) => {
                let raw = std::fs::read_to_string(path)
                    .with_context(|| format!("could not read {}", path.display()))?;
                serde_json::from_str::<Vec<Recipe>>(&raw)
                    .with_context(|| format!("{} is not a JSON array of recipes", path.display()))?
            }
            None => builtin(),
        };
        match table.into_iter().find(|r| r.name == name) {
            Some(r) => Ok(r),
            None => bail!(
                "no recipe named {name}. `shard-tests recipes` lists what is available, and \
                 --recipe-file takes a JSON array of your own."
            ),
        }
    }

    /// Warns when a recipe has never been run against a real suite.
    pub fn announce(&self) {
        match &self.verified {
            Some(where_) => eprintln!("shard-tests: recipe {} (verified: {where_})", self.name),
            None => eprintln!(
                "shard-tests: recipe {} is UNVERIFIED — it has not been executed against a real \
                 suite. Check the units it enumerates before trusting the split.",
                self.name
            ),
        }
    }
}

pub fn builtin() -> Vec<Recipe> {
    vec![
        // Verified against obsidian-remote-ssh: `vitest list --filesOnly` yields 81
        // ids in a deterministic order with no stray lines, and the ids from the
        // JSON reporter join 81/81 with them. `vitest list` is used rather than a
        // glob deliberately: the repo has 107 `*.test.ts` files but the unit config
        // includes only 81, and a glob would hand 26 integration files to shards
        // that cannot run them.
        Recipe {
            name: "vitest".into(),
            build: None,
            enumerate: "npx vitest list --filesOnly".into(),
            test: r#"npx vitest run $SHARD_TESTS_EXTRA $SHARD_TESTS_UNITS --reporter=json --outputFile="$SHARD_TESTS_REPORT""#.into(),
            report: Some(
                r#"jq -r --arg root "$PWD/" '.testResults[] | [(.name | sub("^" + $root; "")), ((.endTime - .startTime) / 1000)] | @tsv' "$SHARD_TESTS_REPORT""#
                    .into(),
            ),
            separator: " ".into(),
            enumerate_needs_build: false,
            transfers: vec![],
            timing_mode: TimingMode::Reported,
            verified: Some("obsidian-remote-ssh, 81 files, reporter ids join 81/81".into()),
            notes: "Units are test files. The reporter's per-file durations summed to 22.6s \
                    against a 24.2s wall clock, so they are a sound balance signal."
                .into(),
        },
        // Verified against QAtlasHub/doiget: 47 distinct test binaries.
        //
        // Units are test *binaries*, not test functions, and that is forced rather
        // than chosen. Function-level `cargo test -- --exact` launches every binary
        // in the workspace per unit (measured: 2.88s each, so 774 units is ~37
        // minutes a shard), and `cargo test -p PKG --test NAME` re-resolves features
        // and rebuilds (measured: 81s) or fails outright when the feature does not
        // exist on that package. Executing the prebuilt binaries directly is the
        // only viable path, which is also why this recipe hands over `target/`.
        // Function-level units need cargo-nextest.
        Recipe {
            name: "cargo-test-binaries".into(),
            build: Some(
                r#"cargo test --workspace --all-targets $SHARD_TESTS_EXTRA --no-run --message-format=json \
 | jq -r 'select(.reason=="compiler-artifact") | select(.profile.test==true) | select(.executable!=null)
     | ((.package_id | capture("(?<n>[a-z0-9_-]+)(@|#)[0-9]").n) // "unknown") as $p
     | [ "\(.target.kind[0])/\($p)/\(.target.name)", (.manifest_path | sub("/Cargo.toml$"; "")), .executable ] | @tsv' \
 > shard-tests-map.tsv"#
                    .into(),
            ),
            enumerate: "cut -f1 shard-tests-map.tsv".into(),
            // Run from the package directory, which is where `cargo test` would put
            // the working directory — a test resolving a fixture by relative path
            // fails anywhere else.
            test: r#"awk -F'\t' -v id="$SHARD_TESTS_UNITS" '$1==id{print $2"\t"$3}' shard-tests-map.tsv \
 | while IFS=$'\t' read -r dir exe; do ( cd "$dir" && "$exe" ); done"#
                .into(),
            report: None,
            separator: "\n".into(),
            enumerate_needs_build: true,
            transfers: vec!["target".into(), "shard-tests-map.tsv".into()],
            timing_mode: TimingMode::Measured,
            verified: Some("QAtlasHub/doiget, 47 test binaries, ids version-free and unique".into()),
            notes: "The id excludes the crate version on purpose: taking it from package_id \
                    verbatim embeds `#0.8.6`, which invalidates every recorded timing on each \
                    release. Sharding cannot take the job below compile time."
                .into(),
        },
    ]
}

/// One line per recipe, for `shard-tests recipes`.
pub fn list() -> String {
    let mut out = String::new();
    for r in builtin() {
        out.push_str(&format!(
            "{name}\n  units come from : {enumerate}\n  build first     : {needs}\n  timing          : {timing}\n  verified        : {verified}\n  {notes}\n\n",
            name = r.name,
            enumerate = r.enumerate,
            needs = if r.enumerate_needs_build {
                "yes — organize runs after build on the first run"
            } else {
                "no — organize can run beside build from the first run"
            },
            timing = match r.timing_mode {
                TimingMode::Reported => "from the runner's own report",
                TimingMode::Measured => "measured per unit by shard-tests",
            },
            verified = r.verified.as_deref().unwrap_or("NOT VERIFIED"),
            notes = r.notes,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_recipe_is_internally_consistent() {
        for r in builtin() {
            assert!(!r.enumerate.is_empty(), "{}: empty enumerate", r.name);
            assert!(!r.test.is_empty(), "{}: empty test", r.name);
            // Reported needs something to read the report from; Measured must not
            // carry one, or the mode is ambiguous about where timings came from.
            match r.timing_mode {
                TimingMode::Reported => {
                    assert!(
                        r.report.is_some(),
                        "{}: Reported without a report hook",
                        r.name
                    )
                }
                TimingMode::Measured => {
                    assert!(
                        r.report.is_none(),
                        "{}: Measured with a report hook",
                        r.name
                    )
                }
            }
            // A recipe that needs the build must say what it hands over, otherwise
            // the shards have nothing to hydrate and would rebuild silently.
            if r.enumerate_needs_build {
                assert!(
                    r.build.is_some(),
                    "{}: needs a build but defines none",
                    r.name
                );
                assert!(
                    !r.transfers.is_empty(),
                    "{}: builds but transfers nothing",
                    r.name
                );
            }
        }
    }

    #[test]
    fn only_verified_recipes_are_shipped() {
        // The shipping policy, enforced rather than remembered: trust is the scarce
        // thing at this stage, so an unverified recipe must not be in the table.
        for r in builtin() {
            assert!(
                r.verified.is_some(),
                "{} ships without verification",
                r.name
            );
        }
    }

    #[test]
    fn find_reports_an_unknown_name() {
        assert!(Recipe::find("no-such-recipe", None).is_err());
        assert!(Recipe::find("vitest", None).is_ok());
    }
}
