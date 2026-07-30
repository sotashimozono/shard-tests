# Recipes

A recipe is three shell snippets. Nothing here is code that `shard-tests` ships or has to
maintain — each entry delegates to the ecosystem's own lister and runner, so a recipe that
rots is a documentation fix, not a release.

**The unit of sharding is whatever `enumerate` prints.** Pick the granularity your suite
can actually be filtered at, and keep the ids stable across runs: recorded timings are
keyed on them.

Note on YAML: a value containing `: ` must be a block scalar (`|`), otherwise the parser
reads the colon as a key separator. Most `enumerate` recipes contain one.

## Rust — function-level

**Read this one first even if you do not write Rust**: it is where the build is expensive
enough to change the topology, and the same reasoning applies to Go, Java and C++.

### Measured, so you can judge whether it is worth it

On a real workspace of 799 tests across 47 test binaries, the `cargo test` step was 167s on
Linux and 309s on Windows, splitting as roughly **92s of compiling and 75s of execution**.
Two consequences:

- Splitting execution cannot take the job below the compile time. At four shards the Linux
  job goes 167s → ~111s and stops improving. Know that before adopting.
- `cargo test` runs test **binaries sequentially** — 47 of them summing 75s. `cargo nextest
  run` puts every test in one pool across binaries, which on that suite cuts execution to
  roughly the longest single binary (~21s) **on one machine, with no sharding at all**. Do
  that first. Sharding is what you reach for when it is not enough.

### Serial: simplest, build in front of the fan-out

Verified against a real suite:

```yaml
enumerate: |
  cargo test --quiet -- --list --format terse | sed -n 's/: test$//p'
run: |
  for u in $SHARD_TESTS_UNITS; do cargo test -- --exact "$u"; done
```

Each shard rebuilds. On a public repository that costs no money — only the compile sitting
in every shard's wall clock.

### Concurrent: build once, transfer it, plan beside it

`nextest` can build to an archive and run from it, so one job compiles and the shards
hydrate. Planning uses `--units-from-timings`, needs no build, and therefore runs *beside*
the build rather than in front of it. Each shard then derives membership from its own
hydrated archive, which is what keeps a newly added test from being lost — see the README
section on this topology.

```yaml
# job build
prepare: cargo nextest archive --archive-file t.tar.zst   # then upload t.tar.zst

# job plan — concurrent with the build
units-from-timings: true
timings: prev-timings.json

# each shard — after downloading t.tar.zst
enumerate: cargo nextest list --archive-file t.tar.zst --message-format json | jq -r '…'
run: cargo nextest run --archive-file t.tar.zst -E "$SHARD_TESTS_UNITS"
separator: ' '
```

> **Unverified:** the `jq` shape for `nextest list --message-format json` is not tested here,
> and nextest's JSON has changed between versions. Run
> `cargo nextest list --message-format json | jq 'keys'` against your own version and build
> the filter from what you see. The `-E` filter expression likewise wants checking against
> your nextest: `test(=name)` terms joined with `+` is the shape to aim for.

## Python — `pytest`, node-level

```yaml
enumerate: pytest --collect-only -q | sed '/^$/,$d'
run: pytest $SHARD_TESTS_UNITS
separator: ' '
```

`--collect-only -q` prints one node id per line and then a blank line followed by a
summary; the `sed` stops at the blank line. Node ids are stable except for parametrised
cases whose ids derive from repr, which drift when the parameters change.

File-level instead, if collection is expensive:

```yaml
enumerate: git ls-files 'tests/**/test_*.py'
run: pytest $SHARD_TESTS_UNITS
separator: ' '
```

## Go — package-level

Packages are already Go's unit of test parallelism, so this is the natural granularity.

```yaml
enumerate: go list ./...
run: go test $SHARD_TESTS_UNITS
separator: ' '
```

Function-level within a package, if one package dominates:

```yaml
enumerate: |
  go test -list '.*' ./... | sed -n '/^Test/p'
run: |
  go test ./... -run "^($(printf '%s|' $SHARD_TESTS_UNITS | sed 's/|$//'))$"
separator: ' '
```

## JavaScript and TypeScript — file-level

Jest, Vitest and Playwright all have a native `--shard`, which is static and equal-sized.
Use a recipe instead when the imbalance between files is what hurts.

```yaml
enumerate: npx vitest list --reporter=json | jq -r '.[].file' | sort -u
run: npx vitest run $SHARD_TESTS_UNITS
separator: ' '
```

## Java — Maven Surefire, class-level

```yaml
prepare: mvn -q -B test-compile
enumerate: |
  find target/test-classes -name '*Test.class' \
    | sed -e 's|target/test-classes/||' -e 's|\.class$||' -e 's|/|.|g'
run: mvn -B surefire:test -Dtest="$SHARD_TESTS_UNITS"
separator: ','
```

## Recording timings

`plan` accepts `--timings`, a JSON object mapping unit id to seconds:

```json
{ "tests/test_api.py::test_login": 12.4, "tests/test_slow.py::test_import": 91.0 }
```

Produce it however your runner reports durations — JUnit XML, a JSON reporter, `/usr/bin/time`
around each unit — and persist it between runs with `actions/cache` or a committed file.
Without it every unit is assumed to take `--default-seconds`, which degrades to splitting by
unit count. `plan` prints what fraction of predicted time is actually measured, so a recipe
whose ids do not match the recorded ones shows up as a low percentage rather than as silently
bad balance.

Converting JUnit XML is not built in yet
([#3](https://github.com/sotashimozono/shard-tests/issues/3)).

## Contributing a recipe

Open a pull request adding a section here. A recipe needs the three snippets, the
granularity it shards at, and one sentence on whether its ids are stable across runs. No
Rust required — nothing in this directory is compiled.
