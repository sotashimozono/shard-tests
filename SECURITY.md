# Security

## Reporting a vulnerability

Use [private vulnerability reporting](https://github.com/sotashimozono/shard-tests/security/advisories/new).
Please do not open a public issue for a vulnerability.

## What this tool can reach

`shard-tests` runs shell snippets that the calling workflow supplies — `prepare`,
`enumerate`, `run` — with the privileges of the job it runs in. It adds no privilege of
its own, but two things are worth knowing when wiring it up:

- **Recipes are code.** Both composite actions pass them through the environment rather
  than interpolating `${{ inputs.… }}` into a shell script, so a recipe cannot break out
  of its own step. Keep it that way if you edit them.
- **A recipe is only as trustworthy as its source.** Do not build one from the title or
  body of a pull request, or from any other attacker-controlled value.

Claim-time assignment ([#1](https://github.com/sotashimozono/shard-tests/issues/1)) will
write git refs to the repository under test, which needs `contents: write`. It is not
implemented yet, and when it is, the static path will remain available for jobs that
should not be granted that.
