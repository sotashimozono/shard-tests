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

## Usage

```yaml
jobs:
  plan:
    runs-on: ubuntu-latest
    outputs:
      matrix: ${{ steps.plan.outputs.matrix }}
    steps:
      - uses: actions/checkout@v7
      - id: plan
        uses: sotashimozono/shard-tests@v1
        with:
          enumerate: pytest --collect-only -q | sed '/^$/,$d'
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
      - uses: actions/download-artifact@v8
        with:
          name: shard-tests-plan
      - uses: sotashimozono/shard-tests/run@v1
        with:
          index: ${{ matrix.index }}
          run: pytest $SHARD_TESTS_UNITS
```

The plan is a separate job because **GitHub Actions cannot build a dynamic
`strategy.matrix` without a prior job's output**. Choosing the shard count from measured
suite size requires it. It also buys two things worth having: enumeration happens once, so
a divergence between shards cannot hide as a silently missing unit, and a broken recipe
fails one job instead of N.

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
