//! Claim-time assignment: a shard takes work when it is free, not when the plan
//! was written.
//!
//! Why, measured rather than assumed. Below the account's concurrency ceiling the
//! jobs of a matrix start together and there is nothing here to win — 0–7s of
//! spread, and static assignment scaling 4.6x on eight shards. Above it the picture
//! inverts: 80 jobs launched at once gave a **37s start spread**, arriving in steps
//! on the job-duration period as earlier jobs freed slots, with an effective ceiling
//! near 31. Static assignment then gets *worse* with more shards — at N=80 each
//! shard holds 1/80th of the work and waits up to 37s to begin, slower than N=8.
//!
//! So the best static shard count is the ceiling, and the ceiling is not knowable
//! from a workflow file: it moves with the plan, the runner type, and whatever else
//! the account is doing. Claiming removes the need to know it. Over-provision; the
//! shards that start drain the queue, and the ones that start late take little or
//! nothing.
//!
//! The primitive is git ref creation, which is a compare-and-swap: measured, a
//! second creation of the same ref loses with 422. It needs `contents: write`,
//! which a pull request from a fork does not have — so capability is **asked up
//! front**, by reading the repository's own `permissions.push`, and the static path
//! taken when it is absent. Discovering it from a 403 in the middle of a run would
//! put a permissions error in the log of the most common contributor's build.
//!
//! Rate limits shape this more than they look like they should. Every shard presents
//! the same token, so they share one secondary-limit budget of roughly eighty
//! content-creating requests a minute, and a claim is one create per unit. That is
//! why the capability question is a read rather than a probe ref — forty shards
//! writing and deleting a probe each would spend the budget before claiming anything
//! — and why throttling is backed off and retried rather than allowed to fail a
//! shard, which would turn a throttle into a red build.
//!
//! Batching matters as much as stealing. Claiming one unit at a time would destroy
//! any runner that parallelises internally, so each round takes
//! `ceil(remaining / shards)` units — large while the queue is full, small as it
//! drains. Guided self-scheduling, and it needs no tuning.

use anyhow::{bail, Context, Result};
use clap::Args;
use std::collections::HashSet;
use std::process::{Command, Stdio};

#[derive(Args)]
pub struct ClaimArgs {}

pub fn main(_args: ClaimArgs) -> Result<()> {
    bail!(
        "`claim` is not a phase of its own — pass --claim to `run`, which claims and executes \
         in one loop. See https://github.com/sotashimozono/shard-tests/issues/1"
    )
}

/// Talks to the refs API with `curl`, which every runner has.
///
/// No HTTP crate on purpose: the binary is downloaded once per shard plus once for
/// the plan, so its size is multiplied by the shard count, and a TLS stack is a
/// large thing to carry for four requests. The token arrives through the
/// environment and is never put on a command line, where the process list would
/// expose it.
pub struct Claimer {
    repo: String,
    namespace: String,
    sha: String,
}

impl Claimer {
    pub fn from_env(run_id: &str) -> Result<Self> {
        let repo = std::env::var("GITHUB_REPOSITORY")
            .context("GITHUB_REPOSITORY is unset — claiming only works inside GitHub Actions")?;
        let sha = std::env::var("GITHUB_SHA").unwrap_or_else(|_| "HEAD".into());
        if std::env::var("SHARD_TESTS_TOKEN").is_err() && std::env::var("GH_TOKEN").is_err() {
            bail!("neither SHARD_TESTS_TOKEN nor GH_TOKEN is set, so nothing can be claimed");
        }
        Ok(Claimer {
            repo,
            namespace: format!("refs/kleroterion/{run_id}"),
            sha,
        })
    }

    /// One request, retried while GitHub is asking us to slow down.
    ///
    /// Necessary rather than defensive. Every shard presents the same token, so they
    /// share one secondary-limit budget of roughly eighty content-creating requests a
    /// minute, and a claim is one create per unit. Forty shards taking forty units sit
    /// right at it. Without backing off, the shard that hits the limit fails the build
    /// — turning a throttle into a red run.
    fn api(&self, args: &str) -> Result<(u32, String)> {
        let mut wait = 2u64;
        for attempt in 1..=5 {
            let (code, body) = self.request(args)?;
            let throttled = (code == 403 || code == 429)
                && (body.contains("rate limit") || body.contains("abuse"));
            if !throttled || attempt == 5 {
                if throttled {
                    eprintln!("shard-tests: still throttled after {attempt} attempts");
                }
                return Ok((code, body));
            }
            eprintln!("shard-tests: GitHub is throttling; waiting {wait}s (attempt {attempt}/5)");
            std::thread::sleep(std::time::Duration::from_secs(wait));
            wait *= 2;
        }
        unreachable!("the loop returns on its last attempt")
    }

    fn request(&self, args: &str) -> Result<(u32, String)> {
        let script = format!(
            r#"token="${{SHARD_TESTS_TOKEN:-$GH_TOKEN}}"
body=$(mktemp)
code=$(curl -sS -o "$body" -w '%{{http_code}}' \
  -H "Authorization: Bearer $token" \
  -H "Accept: application/vnd.github+json" \
  -H "X-GitHub-Api-Version: 2022-11-28" \
  {args})
printf '%s\n' "$code"
cat "$body"
rm -f "$body""#
        );
        let out = Command::new("bash")
            .args(["-eo", "pipefail", "-c", &script])
            .stderr(Stdio::inherit())
            .output()
            .context("could not run curl")?;
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        let (code, body) = text.split_once('\n').unwrap_or((text.trim(), ""));
        let code: u32 = code.trim().parse().unwrap_or(0);
        Ok((code, body.to_string()))
    }

    /// Whether this job can create refs at all.
    ///
    /// Asked once, before any work is assigned, so the absence of the capability is
    /// a mode rather than an error discovered mid-run.
    ///
    /// Asked by *reading* the repository's permissions rather than by writing a probe
    /// ref. GitHub's secondary limit is roughly eighty content-creating requests a
    /// minute, shared by every shard because they all present the same token — so a
    /// probe that creates and deletes a ref per shard spends half that budget before
    /// any work is claimed, and forty shards would exhaust it between them. A read
    /// costs nothing from that budget.
    pub fn can_claim(&self) -> Result<bool> {
        let (code, body) = self.api(&format!(r#""https://api.github.com/repos/{}""#, self.repo))?;
        match code {
            200 => {
                let repo: serde_json::Value =
                    serde_json::from_str(&body).context("the repository response was not JSON")?;
                Ok(repo["permissions"]["push"].as_bool().unwrap_or(false))
            }
            401 | 403 | 404 => Ok(false),
            _ => bail!("unexpected status {code} asking what this token may do: {body}"),
        }
    }

    fn create_ref(&self, name: &str) -> Result<(u32, String)> {
        self.api(&format!(
            r#"-X POST "https://api.github.com/repos/{}/git/refs" \
  -d '{{"ref":"{}","sha":"{}"}}'"#,
            self.repo, name, self.sha
        ))
    }

    fn delete_ref(&self, name: &str) -> Result<()> {
        self.api(&format!(
            r#"-X DELETE "https://api.github.com/repos/{}/git/{}""#,
            self.repo, name
        ))?;
        Ok(())
    }

    /// Unit indices already taken, read in one request.
    pub fn claimed(&self) -> Result<HashSet<usize>> {
        let prefix = self.namespace.trim_start_matches("refs/");
        let (code, body) = self.api(&format!(
            r#""https://api.github.com/repos/{}/git/matching-refs/{}""#,
            self.repo, prefix
        ))?;
        if code == 404 {
            return Ok(HashSet::new()); // nothing claimed yet
        }
        if code != 200 {
            bail!("could not list claims (status {code}): {body}");
        }
        let refs: Vec<serde_json::Value> =
            serde_json::from_str(&body).context("the refs listing was not JSON")?;
        Ok(refs
            .iter()
            .filter_map(|r| r["ref"].as_str())
            .filter_map(|r| r.rsplit('/').next())
            .filter_map(|s| s.parse::<usize>().ok())
            .collect())
    }

    /// Tries to take one unit. True means this shard owns it.
    ///
    /// 422 is the whole design: the second creation of a ref loses, so exactly one
    /// shard can own a unit without any of them coordinating.
    pub fn take(&self, index: usize) -> Result<bool> {
        let (code, body) = self.create_ref(&format!("{}/{index}", self.namespace))?;
        match code {
            201 => Ok(true),
            422 => Ok(false),
            _ => bail!("unexpected status {code} claiming unit {index}: {body}"),
        }
    }
}

impl Claimer {
    /// Removes every claim of this run.
    ///
    /// Claims are per-run and worthless afterwards, and nothing else ever deletes
    /// them — without this the namespace grows by one ref per unit per run, for the
    /// life of the repository.
    pub fn drop_all(&self) -> Result<usize> {
        let taken = self.claimed()?;
        let mut gone = 0;
        for index in &taken {
            if self
                .delete_ref(&format!("{}/{index}", self.namespace))
                .is_ok()
            {
                gone += 1;
            }
        }
        Ok(gone)
    }
}

/// How many units to take in one round: `ceil(remaining / shards)`, at least one.
///
/// Not one-at-a-time, which would serialise any runner that parallelises
/// internally — a suite of 81 files that runs in 24s together takes minutes one
/// file at a time. Not everything-at-once either, which is static assignment with
/// extra steps. Large while the queue is full and small as it drains, which is
/// where the balancing happens.
pub fn chunk_size(remaining: usize, shards: usize) -> usize {
    if remaining == 0 {
        return 0;
    }
    let shards = shards.max(1);
    remaining.div_ceil(shards).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_shrink_as_the_queue_drains() {
        // The property that matters: early rounds are big enough to keep a batching
        // runner batching, late rounds small enough to even the tail out.
        assert_eq!(chunk_size(80, 8), 10);
        assert_eq!(chunk_size(40, 8), 5);
        assert_eq!(chunk_size(8, 8), 1);
        assert_eq!(chunk_size(3, 8), 1);
        assert_eq!(chunk_size(0, 8), 0);
    }

    #[test]
    fn a_single_shard_takes_everything_in_one_round() {
        // Otherwise claiming would add rounds of API calls to the one case where it
        // can win nothing at all.
        assert_eq!(chunk_size(80, 1), 80);
    }

    #[test]
    fn chunking_terminates() {
        // A chunk of zero with work outstanding would spin forever.
        for shards in 1..=16 {
            let mut remaining = 100;
            let mut rounds = 0;
            while remaining > 0 {
                let take = chunk_size(remaining, shards);
                assert!(take > 0, "shards={shards} produced a zero chunk");
                remaining -= take;
                rounds += 1;
                assert!(rounds < 1000, "shards={shards} did not converge");
            }
        }
    }
}
