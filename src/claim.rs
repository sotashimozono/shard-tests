//! The claim phase: assignment decided when a shard asks for work, not when the
//! plan is written.
//!
//! ON-GOING: not implemented. `plan` + `run` provide the static path meanwhile.
//!
//! Why this exists at all: GitHub does not start the jobs of a matrix at the same
//! time. Measured on hosted runners, the spread between the first and last job of
//! one matrix reached 4s–199s. Static assignment hands every shard an equal share
//! as though all of them began at once, so wall clock becomes
//! `max(start delay) + total/N` and the delay dominates. If a shard instead takes
//! its next unit at the moment it is free, a late-starting shard simply takes
//! fewer units, and shard count above the useful minimum stops being a penalty —
//! which is what makes over-provisioning safe when minutes are free, as they are
//! for public repositories.
//!
//! Blocked on one unmeasured fact, tracked in issue #2: a pull request from a
//! fork receives a read-only `GITHUB_TOKEN` and no secrets, so it may not be able
//! to create the git refs the claim uses as its compare-and-swap primitive. Fork
//! pull requests are the dominant contribution path in open source, so the
//! degraded path is not an afterthought — it decides the shape of the protocol.
//! Design work is tracked in issue #1.

use anyhow::{bail, Result};
use clap::Args;

#[derive(Args)]
pub struct ClaimArgs {}

pub fn main(_args: ClaimArgs) -> Result<()> {
    bail!(
        "claim-time assignment is not implemented yet \
         (https://github.com/sotashimozono/shard-tests/issues/1). \
         Use `shard-tests plan` followed by `shard-tests run --index` for the static path."
    )
}
