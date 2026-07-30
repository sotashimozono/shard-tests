//! Execution of caller-supplied recipe hooks.
//!
//! A recipe is a shell snippet written by the workflow author (`prepare`,
//! `enumerate`, `run`). It is passed to the platform shell verbatim: no quoting,
//! word-splitting or interpolation happens here, so a recipe behaves exactly as
//! it would in a `run:` step.

use anyhow::{bail, Context, Result};
use std::process::{Command, Stdio};

/// Runs `script` and returns its stdout. Stderr is inherited so hook diagnostics
/// land in the job log rather than being swallowed.
pub fn capture(script: &str, env: &[(&str, &str)]) -> Result<String> {
    let mut cmd = shell(script);
    for (key, value) in env {
        cmd.env(key, value);
    }
    let output = cmd
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("could not spawn a shell for hook: {script}"))?;
    if !output.status.success() {
        bail!("hook failed ({}): {script}", output.status);
    }
    String::from_utf8(output.stdout).context("hook wrote non-UTF-8 bytes to stdout")
}

/// Runs `script` with stdout and stderr inherited, checking only the exit status.
pub fn status(script: &str, env: &[(&str, &str)]) -> Result<()> {
    let mut cmd = shell(script);
    for (key, value) in env {
        cmd.env(key, value);
    }
    let status = cmd
        .status()
        .with_context(|| format!("could not spawn a shell for hook: {script}"))?;
    if !status.success() {
        bail!("hook failed ({status}): {script}");
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
