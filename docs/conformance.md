# Conformance scorecard

`validation/conformance.py` keeps two different measurements under one JSON
schema without confusing them:

- `builtin-js` and `builtin-web` are GG's small, always-on product contracts.
  CI requires all of them to pass.
- `test262` and `wpt` are adapters for selected files from pinned official
  checkouts. They are labelled `official: true`, and their failures describe
  compatibility gaps rather than being relabelled as GG tests.

The distinction matters. Nine project probes passing is **not** a WPT or
Test262 percentage.

## Local commands

After installing the native `ggcore` wheel:

```powershell
python validation/conformance.py
python validation/conformance.py `
  --test262-root C:\src\test262 `
  --wpt-root C:\src\wpt `
  --baseline validation/baselines/official-conformance.json
```

The first command runs the 9-test release gate and writes
`validation/out/conformance.json`. The second also expands the paths and globs
in `validation/conformance_subsets/` against local upstream checkouts. Each
official test runs in a child process with `--case-timeout`, so an engine loop
cannot hang the whole scorecard. When `--max-tests` caps a multi-pattern
selection, files are taken round-robin so one large feature directory cannot
consume the entire budget.

Use `--strict` only when every selected upstream failure should fail the
command. Normally the checked baseline gates tests that previously passed,
while known failures remain visible and newly passing tests are reported as
improvements. Refresh a baseline intentionally with:

```powershell
python validation/conformance.py <same suite arguments> --update-baseline
```

Review the diff before committing it; accepting a `pass` to `fail` change would
erase a regression signal.

Official baselines also store each checkout revision, the selection-file hash,
and the test cap. A run against different inputs stops as a configuration error
instead of comparing unrelated scores.

## Adapter boundaries

The Test262 adapter follows the suite frontmatter, fresh-realm, harness,
strict/sloppy, negative, and async rules. Module resolution, agents, and
advanced `$262` host hooks are currently recorded as `skip` with a reason.
This matches the official execution contract instead of counting unsupported
runner infrastructure as an engine failure. See
[Test262's interpreting guide](https://github.com/tc39/test262/blob/main/INTERPRETING.md).

The WPT adapter currently handles static HTML `testharness.js` tests. It
inlines checkout-local script dependencies and records testharness subtest and
completion callbacks. Server-generated `.py` resources, variants, remote
scripts, worker globals, and reftests need the future HTTP/visual adapters.
The upstream project and its normal `wpt serve`/`wpt run` tools are documented
in the [WPT repository](https://github.com/web-platform-tests/wpt).

The scheduled GitHub workflow pins both upstream commit SHAs, uses sparse
checkouts, caps the selected Test262 run, and uploads the complete JSON report.

The initial pinned baseline is 37/120 Test262 files and 0/2 static WPT files,
plus 9/9 project contract probes. The baseline is intentionally small and
trend-oriented; it is not a percentage for either complete upstream suite.
