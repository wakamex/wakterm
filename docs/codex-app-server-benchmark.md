# Codex app-server topology benchmark

Wakterm runs one mux-owned Codex app-server and connects each native Codex TUI
to it. `scripts/bench_codex_topology.py` compares that topology with one
app-server per TUI.

The benchmark creates 20 isolated project directories and 20 durable named
Codex threads. It starts 20 native TUI clients but sends no prompts, so the
result has no model traffic or API latency. Every topology uses one isolated
Codex home, matching the shared state directory used by normal parallel Codex
sessions.

Measured on 2026-08-24:

- AMD Ryzen 9 3950X, 32 logical CPUs, 31 GiB RAM
- Fedora 44, Linux 7.1.9
- Codex `0.148.0-alpha.20+upstream.21cfd369ef`
- 10-second settle, 65-second sample, median of three runs

| Total for 20 idle TUIs | Per-pane servers | Shared server | Reduction |
| --- | ---: | ---: | ---: |
| PSS | 865.90 MiB | 464.04 MiB | 46.4% |
| Private memory | 782.19 MiB | 415.95 MiB | 46.8% |
| Processes | 40 | 21 | 47.5% |
| Threads | 1,765 | 944 | 46.5% |
| File descriptors | 1,550 | 858 | 44.6% |
| One-core CPU | 2.65% | 0.29% | 89.2% |
| Read syscalls | 92,826 | 9,797 | 89.4% |
| Write syscalls | 46,797 | 4,689 | 90.0% |
| Voluntary context switches | 82,305 | 8,168 | 90.1% |

The app-server portion alone fell from 482.0 MiB to 60.6 MiB PSS. The 20 TUI
clients remain separate processes in both cases. Sharing removes 19 redundant
provider servers and their idle database, watcher, runtime, and IPC work; it
does not combine the terminal frontends.

Run it with:

```sh
uv run --script scripts/bench_codex_topology.py \
  --harnesses 20 --runs 3 \
  --settle-seconds 10 --sample-seconds 65 --sample-interval 5 \
  --output codex-topology-20.json
```

The script alternates topology order across repeated runs. It measures each
process forest through `/proc` and `smaps_rollup`, keeps app-server and TUI
totals separate, and emits every raw run as JSON.

This provider-level saving currently applies to Codex because Wakterm has a
shared Codex app-server integration. Agy, Claude, Gemini, and OpenCode still
run as independent PTY harnesses. They benefit from the small mux overhead and
idle event infrastructure measured in the [headless mux benchmark](headless-mux-benchmark.md),
but Wakterm does not claim provider-process consolidation for them.
