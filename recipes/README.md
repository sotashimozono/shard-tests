# Recipes

A recipe is three shell snippets. Nothing here is code that `shard-tests` ships or has to
maintain — each entry delegates to the ecosystem's own lister and runner, so a recipe that
rots is a documentation fix, not a release.

**The unit of sharding is whatever `enumerate` prints.** Pick the granularity your suite
can actually be filtered at, and keep the ids stable across runs: recorded timings are
keyed on them.

Note on YAML: a value containing `: ` must be a block scalar (`|`), otherwise the parser
reads the colon as a key separator. Most `enumerate` recipes contain one.

## Rust — `cargo nextest`, function-level

`nextest` can build once and run from an archive, which is what makes the plan job pay for
itself: the shards reuse its artifact instead of each compiling the suite.

```yaml
prepare: cargo nextest archive --archive-file target/nextest.tar.zst
enumerate: |
  cargo nextest list --archive-file target/nextest.tar.zst --message-format json \
    | jq -r '."rust-suites" | to_entries[] | .value.testcases | keys[] as $t | "\(.[$t].name)"'
run: |
  cargo nextest run --archive-file target/nextest.tar.zst \
    -E "$(printf 'test(=%s) + ' $SHARD_TESTS_UNITS | sed 's/ + $//')"
separator: ' '
```

Plain `cargo test` works too, at the cost of rebuilding per shard:

```yaml
enumerate: |
  cargo test --quiet -- --list --format terse | sed -n 's/: test$//p'
run: |
  for u in $SHARD_TESTS_UNITS; do cargo test -- --exact "$u"; done
```

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
