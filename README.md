# shard-tests

Split any test suite across GitHub Actions runners. Timing-balanced, language-agnostic,
**no server and no account**.

A public repository gets GitHub-hosted runners at no cost, with no minute limit and an
account-wide concurrency allowance ([current limits][limits]). A slow suite on a public
repository is therefore not a budget problem, it is a scheduling problem — and the tools
that schedule well are mostly commercial and need a server. This one needs neither: the
only state it keeps lives in your own repository.

[limits]: https://docs.github.com/actions/reference/limits

## Status

**v0.1 — the static path works.** `plan` + `run` split any suite by measured per-unit
time, on any language, and that alone is a complete tool.

**Claim-time assignment is not implemented yet** ([#1](https://github.com/sotashimozono/shard-tests/issues/1)).
That is the part this project exists for, and it is deliberately not faked: until it
lands, `shard-tests` is a static splitter and says so.

## The idea

GitHub does not start the jobs of a matrix at the same time. Measured on hosted runners,
the spread between the first and last job of one matrix reached **4s to 199s**. Static
splitting — every existing tool in this space, and every native `--shard` flag — hands
each shard an equal share as though all of them began at once. Wall clock becomes

```
max(start delay)  +  total / N
```

and when the suite is fast the stagger dominates completely. Adding shards makes it
worse, because each new shard pays fixed job overhead and is exposed to the same stagger.

If a shard instead takes its next unit **at the moment it is free**, a late-starting shard
simply takes fewer units, or none. Shard count above the useful minimum stops being a
penalty. And when minutes are free — as they are for public repositories — that is the
whole game: **you can over-provision, and over-provisioning is only safe under
claim-time assignment.**

## Three hooks, any language

The binary knows nothing about any test framework. A recipe supplies three shell snippets,
and **the unit of sharding is whatever `enumerate` prints** — a file, a test function, a
package, a test set. That is what lets file-per-unit, function-per-unit and
package-per-unit ecosystems share one implementation.

| hook | when | why |
| --- | --- | --- |
| `prepare` | once, in the plan job | compiled languages cannot list tests without building; the artifact it leaves is what shards reuse instead of each building their own |
| `enumerate` | once, in the plan job | prints one stable unit id per line |
| `run` | per shard | receives its ids in `$SHARD_TESTS_UNITS` |

```
# Rust
prepare:   cargo nextest archive --archive-file target/nextest.tar.zst
enumerate: cargo nextest list --archive-file target/nextest.tar.zst --message-format json | jq -r '...'
run:       cargo nextest run --archive-file target/nextest.tar.zst -E "$SHARD_TESTS_UNITS"

# Python
enumerate: pytest --collect-only -q | sed '/^$/,$d'
run:       pytest $SHARD_TESTS_UNITS

# Go
enumerate: go list ./...
run:       go test $SHARD_TESTS_UNITS
```

See [`recipes/`](recipes/) for the full set.

## Recipes

A recipe supplies the hooks, the separator, and the properties that decide how the jobs
have to be assembled. `shard-tests recipes` prints what ships and **where each one was
actually executed** — only recipes that have been run against a real suite are included,
because a recipe nobody has run reads as support and behaves as a bug report.

| recipe | units | build first | durations |
| --- | --- | --- | --- |
| `vitest` | test files | no — planning runs beside the build | from the runner's report |
| `cargo-test-binaries` | test binaries | yes | timed per unit by shard-tests |

Anything passed explicitly beats the recipe, so a recipe is a default and not a wall.
Coverage flags and the like go through `extra`, which reaches the hooks as
`$SHARD_TESTS_EXTRA` — no rewriting required. Hooks inherit the job's environment, so a
secret belongs in the step's `env:` and referenced from there, never written into a recipe.
`--recipe-file` takes a JSON array of your own if none of the built-ins fit.

## Usage

```yaml
jobs:
  plan:
    runs-on: ubuntu-latest
    outputs:
      matrix: ${{ steps.plan.outputs.matrix }}
    steps:
      - uses: actions/checkout@v7
      - run: npm ci
      - id: plan
        uses: sotashimozono/shard-tests@v1
        with:
          recipe: vitest
          target-seconds: 60
          max-shards: 8
      - uses: actions/upload-artifact@v7
        with:
          name: shard-tests-plan
          path: shard-tests-plan.json

  test:
    needs: plan
    runs-on: ubuntu-latest
    strategy:
      fail-fast: false
      matrix: ${{ fromJSON(needs.plan.outputs.matrix) }}
    steps:
      - uses: actions/checkout@v7
      - run: npm ci
      - uses: actions/download-artifact@v8
        with:
          name: shard-tests-plan
      - uses: sotashimozono/shard-tests/run@v1
        with:
          recipe: vitest
          index: ${{ matrix.index }}
```

The plan is a separate job because **GitHub Actions cannot build a dynamic
`strategy.matrix` without a prior job's output**. Choosing the shard count from measured
suite size requires it. It also buys two things worth having: enumeration happens once, so
a divergence between shards cannot hide as a silently missing unit, and a broken recipe
fails one job instead of N.

## If your suite has to be built first

`enumerate` normally needs the build, which puts the build in front of the fan-out. To build
once and split only the execution, plan from timings instead and let each shard enumerate:

```
job build:  build once → artifact                 ─┐
                                                   ├→ N shards: hydrate, run their slice
job plan:   plan --units-from-timings              ┘   ← concurrent with the build
```

```yaml
      - uses: sotashimozono/shard-tests/run@v1
        with:
          index: ${{ matrix.index }}
          enumerate: <list from the hydrated artifact>
          run: <run this shard's units>
```

`--units-from-timings` needs no build, so planning runs beside one. Its universe is only a
**prediction**, so pair it with `run --enumerate`: each shard derives membership from its own
artifact, and because assignment is a deterministic function of (universe, durations, shard
count) the shards reach the same partition without coordinating. A unit runs exactly once,
and one the plan never predicted is assigned rather than silently skipped — the difference is
reported as drift. Large drift means the shard count came from a stale total; refresh the
timings.

## Recording durations

Balance needs to know what each unit costs, and the first run cannot. So the first run
splits by unit count, records what it observed, and every run after that balances on
measurement.

```yaml
      - uses: sotashimozono/shard-tests/run@v1
        with:
          recipe: vitest
          index: ${{ matrix.index }}
          runner: ubuntu-latest
          timings-out: shard-${{ matrix.index }}.jsonl
```

then once, after the shards:

```bash
shard-tests finalize shard-*.jsonl --store timings.jsonl --universe units.txt
```

and the next `plan` reads it with `--timings timings.jsonl --runner ubuntu-latest`.

The store is **append-only JSONL**, one observation per line, which is why there is no merge
step to get wrong: N shards each write their own lines and the store is their concatenation.
Provenance is a field rather than a filename, so one store holds every platform and the
reader selects — a Windows job can be twice a Linux one, and balancing across both balances
neither. Keeping the observations rather than a smoothed number means `plan` takes the median
of the most recent few at read time, so a single cold-cache run does not move the estimate,
and the policy can change later without the raw numbers having been destroyed. `finalize`
trims to the last few per unit and drops units no longer in the universe, so a deleted test
stops counting toward the total that sets the shard count.

Where the store lives between runs is the workflow's business, not this tool's: `--timings`
takes a path. A cache, an artifact, a committed file, or a git ref all work. A ref has the
property that a pull request from a fork can read it even though it cannot write — which
matches trunk being the only thing that should update it.

## Prior art, honestly

Static, timing-balanced, file-level splitting is already solved several times over, and
those tools are good. What none of them do is decide assignment at claim time.

| | assignment | unit granularity | needs a server |
| --- | --- | --- | --- |
| native `--shard` (Jest, Vitest, Playwright, [nextest][nextest] `--partition`) | static, equal | fixed by the framework | no |
| [split-tests][st], [split-tests-by-timings][r7k], [split_tests][ls] | static, timing-balanced | test files | no |
| [Shopify/ci-queue][ciq] | **claim time** | framework-specific | **yes** (Redis) |
| Knapsack Pro Queue Mode | **claim time** | framework-specific | **yes** (hosted) |
| `shard-tests` | claim time ([#1][i1]), static fallback | whatever `enumerate` prints | **no** |

If you want static file-level splitting today and nothing else, use one of the tools in
the second row — they are smaller and they work. This one is for the case where the
stagger is what is costing you, or where your units are not files.

Timing input is a JSON object mapping unit id to seconds, so migrating from any of them
is a format conversion, not a rewrite.

[nextest]: https://nexte.st
[st]: https://github.com/scruplelesswizard/split-tests
[r7k]: https://github.com/r7kamura/split-tests-by-timings
[ls]: https://github.com/leonid-shevtsov/split_tests
[ciq]: https://github.com/Shopify/ci-queue
[i1]: https://github.com/sotashimozono/shard-tests/issues/1

## Related

[`TestShards.jl`](https://github.com/QAtlasHub/TestShards.jl) is the Julia-native sibling.
Julia has no `--shard` flag and no cheap way to list test files ahead of time, so it shards
by intercepting `include`; that trick is Julia-specific and stays there.

## License

MIT
