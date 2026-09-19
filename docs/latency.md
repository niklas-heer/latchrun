# Measuring command latency

## Recorded macOS baseline

Measured on 2026-09-19 with release Latchrun 0.1.0, Rust 1.97.1, Apple M4 (10 effective threads), macOS 27.0 / Darwin 27.0.0, arm64, and APFS. Production code was revision `128c77a`; the working tree also contained the benchmark and test/CI changes. Runtime and analytics storage used a private fixture on the local `/tmp` filesystem. No concurrent build or second benchmark was run.

Each row contains 100 measured pairs after five warmups. All times are milliseconds. Added latency is calculated separately for each pair; its median therefore need not equal the difference between the two absolute medians.

| Case | Direct median | Latchrun median | Added median | Added p95 | Added p99 |
| --- | ---: | ---: | ---: | ---: | ---: |
| No credentials | 1.304 | 55.053 | 53.483 | 60.961 | 96.266 |
| Fake credential, per command | 1.295 | 55.121 | 53.777 | 62.882 | 79.390 |
| Fake credential, warm cache | 1.332 | 55.104 | 53.798 | 64.571 | 69.482 |
| 20 ms provider fixture, uncached | 1.338 | 93.650 | 91.909 | 101.565 | 115.802 |
| 20 ms provider fixture, warm cache | 1.426 | 56.398 | 54.970 | 65.134 | 69.092 |
| OS sandbox, no credentials | 1.370 | 65.765 | 64.421 | 77.699 | 198.383 |

The 20 fresh service starts had median readiness **16.358 ms**, p95 **18.263 ms**, and p99 **18.314 ms**. This includes the readiness probe described below.

Accounting verified all **630 operations succeeded**, with no denied, failed, unknown, or running outcomes. The two cached cases produced **208 cache hits and two initial misses**. All 600 measured paired differences were nonnegative. Outliers were retained: the largest sandbox added-latency sample was 572.775 ms; the largest direct sample across cases was 60.344 ms. These tails are part of the observed run, not a guaranteed bound.

The local raw artifact is `scratch/latency-macos-1789839571.json`. Its sample counts, paired subtraction, and nearest-rank percentiles were checked against the recorded arrays. This is one host/run, not a Linux result or a live credential-provider measurement.

The implementation currently uses 10 ms polling in the service accept loop and worker/provider supervision. Those intervals are plausible contributors to short-command overhead, alongside process startup, cleanup, and synchronous persistence. This benchmark does not profile those components, so it cannot assign a measured fraction to any one of them. No production behavior was changed for this baseline.

## Reproduction and interpretation

Run `mise run bench` on an otherwise idle machine. This builds the release binary and runs the public CLI against private, temporary runtime and data directories. It uses only fake credentials and does not contact a credential manager. Linux requires the same working bubblewrap backend as the sandbox tests; unavailable enforcement fails the benchmark.

The default run measures 100 pairs for each of six cases after five warmups per case:

- No credentials.
- An in-process fake credential resolved for every command.
- A warm fake credential cache.
- A fixture executable that sleeps 20 ms before returning a fake credential, without caching.
- The same fixture with a warm cache.
- No credentials with the platform OS sandbox enabled.

Every pair compares `/usr/bin/true` directly with the same executable through `latchrun run`. Both child environments contain `PATH=/usr/bin:/bin` and `LANG=C`; credential cases include the same fake `TOKEN` value. The working directory matches. Both measured launches use null stdin/stdout and capture empty stderr. Direct execution includes process creation and waiting. The Latchrun measurement additionally includes client startup, IPC, authorization, credential resolution when applicable, worker supervision, output handling, and normal journal/SQLite persistence. It excludes initial service startup and session creation.

Cases rotate in round-robin order and pairs alternate which path runs first. No samples or negative differences are discarded. Reported added latency is the distribution of the signed **per-pair difference**, not a subtraction of independently calculated percentiles. Median, p95, and p99 use the exact nearest rank. Raw nanosecond samples are included so noise and changes over the run can be inspected.

All six cases share one service. Its operation ledger grows from 30 entries after warmup to 630 entries at the default end; no pruning occurs inside timed runs. This includes the real cost of persisting a growing session history. The result is not a constant-cost guarantee for another history size, filesystem, machine, command, or concurrent workload.

Twenty separate readiness measurements create fresh service runtime/data directories. They time a foreground `service serve` spawn through the first successful public `service status`, with a 1 ms sleep between unsuccessful probes. These are fresh service starts, **not flushed disk or executable-cache measurements**. Readiness polling and probe process startup are included.

The JSON artifact defaults to `scratch/latency-<os>-<timestamp>.json` (Git ignored). It records OS/kernel, CPU architecture/model, effective thread count, filesystem, toolchain/build version, source revision, sample counts, cache accounting, absolute/direct latency, paired overhead, and raw samples. It contains no machine hostname, home path, command output, real credentials, or provider references. The fixture's 20 ms delay is artificial; it does not measure live 1Password network, unlock, or user interaction time.

Optional environment variables:

| Variable | Default | Purpose |
| --- | --- | --- |
| `LATCHRUN_BENCH_SAMPLES` | `100` | Samples per case, from 1 to 165; the upper bound keeps the ledger below 1,024 entries. |
| `LATCHRUN_BENCH_COLD_SAMPLES` | `20` | Fresh service starts, from 0 to 100. |
| `LATCHRUN_BENCH_OUTPUT` | generated path under `scratch/` | JSON output path; an existing file is never overwritten. |

Run native macOS and Linux measurements separately, without concurrent builds or benchmarks. Compare results only with their recorded environment and methodology. The benchmark cleans up its service processes and temporary directories after success, errors, SIGINT, or SIGTERM.
