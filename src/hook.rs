//! Execution of caller-supplied recipe hooks.
//!
//! A recipe is a shell snippet written by the workflow author (`build`, `enumerate`,
//! `test`, `report`). It is passed to the platform shell verbatim: no quoting, word
//! splitting or interpolation happens here, so a recipe behaves exactly as it would
//! in a `run:` step.
//!
//! The hook inherits the job's environment, which is how a secret reaches a runner
//! that needs one — put it in the step's `env:` and reference it from the recipe.
//! Failures are reported by the hook's **role**, never by echoing the snippet: a
//! recipe that had a credential written into it would otherwise print it on every
//! failure, and diagnosing which hook broke does not need the text (`shard-tests
//! recipes` shows it).

use anyhow::{bail, Context, Result};
use std::process::{Command, Stdio};

/// Runs `script` and returns its stdout. Stderr is inherited so hook diagnostics
/// land in the job log rather than being swallowed.
pub fn capture(role: &str, script: &str, env: &[(&str, &str)]) -> Result<String> {
    let mut cmd = shell(script);
    for (key, value) in env {
        cmd.env(key, value);
    }
    let output = cmd
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("could not spawn a shell for the {role} hook"))?;
    if !output.status.success() {
        bail!("the {role} hook failed ({})", output.status);
    }
    String::from_utf8(output.stdout)
        .with_context(|| format!("the {role} hook wrote non-UTF-8 bytes to stdout"))
}

/// Runs `script` with stdout and stderr inherited, checking only the exit status.
pub fn status(role: &str, script: &str, env: &[(&str, &str)]) -> Result<()> {
    let mut cmd = shell(script);
    for (key, value) in env {
        cmd.env(key, value);
    }
    let status = cmd
        .status()
        .with_context(|| format!("could not spawn a shell for the {role} hook"))?;
    if !status.success() {
        bail!("the {role} hook failed ({status})");
    }
    Ok(())
}

#[cfg(windows)]
fn shell(script: &str) -> Command {
    let mut cmd = Command::new("pwsh");
    cmd.args(["-NoProfile", "-Command", script]);
    cmd
}

#[cfg(not(windows))]
fn shell(script: &str) -> Command {
    let mut cmd = Command::new("bash");
    // `pipefail` matters: `enumerate` recipes are usually pipelines ending in a
    // filter, and without it a failing lister upstream looks like an empty suite.
    cmd.args(["-eo", "pipefail", "-c", script]);
    cmd
}
