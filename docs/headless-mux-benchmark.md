# Headless mux idle benchmark

The repeatable workload in `scripts/bench_restored_mux.py` measures the mux and
its PTY children without a GUI client. It reports proportional set size (PSS),
RSS, private memory, swap, process and thread counts, file descriptors, CPU
time, context switches, faults, and read and write syscalls.

## Representative restored session

The restored fixture is based on the shape of the development host's active
session on 2026-08-24, scaled from 12 to 20 harness workspaces:

- 20 tabs, each with one harness-shaped PTY
- 7 tabs with a second shell PTY, for 27 PTYs total
- 8 parked tabs
- one window and one workspace
- deterministic `sleep infinity` payloads with empty scrollback

The fixture exercises persisted tab order, split trees, active panes, parked
tabs, PTY creation, and the first periodic save. It does not launch an agent
provider. Provider topology has its own benchmark because Codex memory would
otherwise hide the mux result.

Run it with release binaries:

```sh
uv run --no-project scripts/bench_restored_mux.py \
  --setup restore \
  --server-bin target/release/wakterm-mux-server \
  --cli-bin target/release/wakterm \
  --tabs 20 --split-tabs 7 --parked-tabs 8 \
  --runs 3 --settle-seconds 10 --sample-seconds 65 \
  --output wakterm-restored.json
```

The 65-second sample intentionally crosses Wakterm's 60-second persistence
interval. The sampler reads `/proc` and `smaps_rollup`; it does not poll the mux
during the idle window.

Median of three runs on the host described below:

| Metric | Result |
| --- | ---: |
| Restore 20 tabs and 27 PTYs | 149.0 ms |
| Mux PSS | 30.94 MiB |
| Mux plus 27 PTYs PSS | 34.44 MiB |
| Mux private memory | 30.89 MiB |
| Mux CPU time during sample | 1.05 ms |
| One-core CPU | 0.0016% |
| Read and write syscalls | 7 and 5 |
| Voluntary context switches | 4 |
| Major faults and swap | 0 and 0 |

## Wakterm and upstream WezTerm

The cross-project comparison uses the same cold spawn workload because
Wakterm's v5 persisted layout is a fork-specific input. Each server gets 20
tabs with one sleeping PTY per tab. This compares headless mux and PTY steady
state, not restoration behavior, terminal rendering, GPU memory, active
output, or scrollback retention.

Measured on 2026-08-24:

- AMD Ryzen 9 3950X, 32 logical CPUs, 31 GiB RAM
- Fedora 44, Linux 7.1.9
- rustc 1.95.0, release builds
- Wakterm `6583a2e85`
- upstream WezTerm `f93d90350`
- 10-second settle, 65-second sample, median of three runs

| Metric | Wakterm | WezTerm |
| --- | ---: | ---: |
| Cold setup | 558.7 ms | 532.7 ms |
| Mux PSS | 29.62 MiB | 32.67 MiB |
| Mux plus 20 PTYs PSS | 32.15 MiB | 35.20 MiB |
| Mux private memory | 29.58 MiB | 32.63 MiB |
| CPU time during sample | 0.76 ms | below measurement resolution |
| One-core CPU | 0.0012% | below measurement resolution |
| Mux threads | 95 | 86 |
| Mux file descriptors | 116 | 110 |
| Swap | 0 | 0 |

Wakterm used 3.05 MiB less PSS for both the mux and full process tree in this
workload. Its cold setup took 26 ms longer. The measured Wakterm CPU includes
the periodic persistence pass and is still about one thousandth of one CPU
core.

Run the comparison with the same script and an upstream release build:

```sh
uv run --no-project scripts/bench_restored_mux.py \
  --flavor wakterm --setup spawn \
  --server-bin target/release/wakterm-mux-server \
  --cli-bin target/release/wakterm \
  --tabs 20 --runs 3 --settle-seconds 10 --sample-seconds 65

uv run --no-project scripts/bench_restored_mux.py \
  --flavor wezterm --setup spawn \
  --server-bin /path/to/wezterm-mux-server \
  --cli-bin /path/to/wezterm \
  --tabs 20 --runs 3 --settle-seconds 10 --sample-seconds 65
```

PSS is the primary memory number because it divides shared mappings among the
processes that use them. Summed RSS is also retained in the JSON output, but it
double-counts shared pages.

## Tab navigator resource sampling

The ignored release workload `bench_tab_resource_status_24_tabs` profiles the
resource snapshot used by the tab navigator. It creates 24 tabs with one live
sleeping process each. The navigator refreshes every five seconds while open.

Five untraced runs on the same host produced these medians:

| Work | Result |
| --- | ---: |
| Cold process and RSS snapshot | 9.20 ms |
| Cold process CPU | 9.07 ms |
| Warm process-cache snapshot | 0.184 ms |
| Retained RSS after cold snapshot | 56 KiB |
| Current row RSS lookups | 1.65 us per navigator refresh |
| One-snapshot row RSS lookups | 0.39 us per navigator refresh |
| Voluntary context switches | 0 |
| Major faults | 0 |

The cold path scanned 548 host processes and then sampled 24 pane roots. A
focused `strace` recorded 597 `openat`, 596 `statx`, 2,933 `read`, 48
`readlink`, and 2 `getdents64` calls. At one refresh every five seconds, the
9.20 ms sample averages 0.18% of one core while the navigator is open.

Cloning the cached RSS map once per row is 4.2 times slower than taking one
snapshot, but the absolute difference is 1.26 microseconds per five-second
refresh. The process scan dominates, and its measured load is too small to
justify a separate discovery path or a longer stale-data window.

Run the workload with:

```sh
cargo test -p mux bench_tab_resource_status_24_tabs \
  --release -- --ignored --nocapture
```
