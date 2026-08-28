# Dependency-tool benchmarks

This harness supports the dated results in
[`docs/comparison.md`](../docs/comparison.md). It compares workloads, not product
categories. A tool is included only when it can perform the same useful operation
non-interactively on the same fixture.

## Workloads

The committed dataset is two synthetic files totaling 49 lines and 980 bytes.
Keeping the fixtures small makes the workload primarily a measure of registry
and GitHub release discovery rather than repository traversal.

| Fixture | Dependencies | Lines | Size |
|---|---:|---:|---:|
| [`fixtures/vendor/python/pyproject.toml`](fixtures/vendor/python/pyproject.toml) | 12 Python requirements | 18 | 361 bytes |
| [`fixtures/actions/.github/workflows/ci.yml`](fixtures/actions/.github/workflows/ci.yml) | 6 GitHub Actions | 31 | 619 bytes |
| **Total** | **18 references** | **49** | **980 bytes** |

### Python manifest constraints

The fixture contains twelve exact direct requirements in project dependencies.
It lives below a `vendor/` path so GitHub's dependency graph does not mistake
its deliberately obsolete inputs for shipped dependencies. Repository-wide
`upd` scans exclude the same path through `.updrc.toml`; the benchmark passes
the manifest explicitly so the fixture remains fully exercised.
The check benchmark discovers newer PyPI releases without writing. The update
benchmark starts from the same pristine `pyproject.toml` for every run and
writes newer exact constraints. `uppd` uses its documented separate-output mode
because version 1.6.0 appends an extra closing bracket when overwriting this
fixture in place; the verifier rejects that malformed default output rather
than timing it as success.

Compared tools:

- `upd`
- `uppd`

uv and PDM primarily resolve lockfiles; uvu requires interactive selection;
uv-upx intentionally skips exact pins. They are documented in the feature
matrix but excluded from this workload.

### GitHub Actions releases

The fixture contains six tagged Actions. The check benchmark resolves available
updates without writing the fixture. The update benchmark starts from a pristine
workflow for every run.

Compared tools:

- `upd`
- pinact
- actions-up
- taze
- ratchet

The tools do not produce identical text: some preserve a tag, while others pin a
resolved commit and retain a tag in a comment. All perform the user-visible job
of finding and applying newer Action releases. The result summary calls out this
difference instead of treating the output styles as identical.

## Requirements

- macOS or Linux on arm64 or x86-64
- Rust/Cargo matching the repository toolchain
- uv
- Node.js and npm
- Hyperfine
- curl, `awk`, `tar`, and `shasum`
- `GITHUB_TOKEN` or `GH_TOKEN`; authenticated GitHub API access is required so
  rate-limit exhaustion cannot turn a benchmark run into an error benchmark

## Install exact tool versions

Build `upd`, then install the comparison tools into an ignored local directory:

```bash
cargo build --release
./benchmarks/install-tools.sh
export PATH="$PWD/benchmarks/.tools/bin:$PATH"
```

The installer reads [`versions.env`](versions.env), uses a generic HTTP user
agent, verifies the published pinact SHA-256 and ratchet SHA-512 checksums, and
keeps Python/npm tool environments out of the timed commands.

Before timing, [`verify-outputs.sh`](verify-outputs.sh) runs every update command
once and checks the result: both Python tools must raise all 12 exact constraints
and leave parseable TOML; every Actions tool must change all 6 references while
retaining the workflow structure. The verification transcript is stored beside
the raw timing data.

## Run

```bash
./benchmarks/run.sh
```

Optional controls:

```bash
BENCH_RUNS=10 BENCH_WARMUP=2 ./benchmarks/run.sh
BENCH_RESULT_DIR=/tmp/upd-benchmark-results ./benchmarks/run.sh
UPD_BIN=/path/to/upd ./benchmarks/run.sh
BENCH_VERIFY=0 ./benchmarks/run.sh # timing only; skips output verification
```

The default is one warm-up followed by five measured runs. Raw Hyperfine JSON
and an environment file are written under `benchmarks/results/raw/`. Raw runs
are ignored by Git; a reviewed dated summary and the corresponding selected raw
JSON can be committed when refreshing the published results.

## Methodology and limitations

- Hyperfine restores the relevant fixture before every command, including each
  warm-up.
- `uppd` writes `updated.toml`; `upd` updates the restored fixture in place. Both
  results must pass the same TOML and 12-change verification.
- Tool installation and package-manager startup are excluded; CLI startup,
  parsing, registry/API work, comparison, and writing are included.
- `upd --no-cache` disables its 24-hour semantic version cache. Other tools are
  invoked without a persistent result cache where their documented CLI permits
  it. Package installation caches do not matter because installation is outside
  the timed command.
- Registry and GitHub API responses are live. DNS, TLS, CDN latency, upstream
  rate limits, and server-side caches remain sources of variance. Treat results
  as observations of the recorded environment, not universal constants.
- Update output is verified automatically before timing. This catches commands
  that exit successfully without performing the intended workload; the reviewed
  dated summary still records any output-policy differences.
- Commands that intentionally signal pending updates with exit code `1` are
  wrapped with an exact status assertion. An unexpected success or any error
  status stops Hyperfine instead of measuring an error path.
- Renovate and Dependabot CLI are excluded because their normal update jobs add
  repository, container, branch, and PR-operation work. Timing that workflow
  against a local editor would not isolate dependency checking.
- Memory is not published by the portable harness. Hyperfine may emit
  `memory_usage_byte` on some platforms, but the collection method is not
  consistent across operating systems; a future containerized benchmark may
  add a comparable peak-RSS measurement.
