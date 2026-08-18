# Codex app-server topology benchmark

Measured on 2026-08-18 on the Fedora development host.

- Codex: `codex-cli 0.148.0-alpha.20+upstream.21cfd369ef`
- Wakterm: `0.1.0`, shared-app-server working tree based on `088c606a5`
- Provider processes used the installed optimized Codex build. Wakterm release
  artifacts were built with `cargo build --release -p wakterm
  -p wakterm-mux-server-impl`; mux RSS is not included in the provider totals.
- Commands: `codex app-server --listen unix://PATH` and `codex resume
  --remote unix://PATH --no-alt-screen -a never -s read-only THREAD_ID`
- Each thread had a separate temporary working directory.
- Process counts and RSS include each process tree rooted at the app-server or
  TUI process. App-server and TUI measurements are separate.
- Each case settled for 8 seconds and sampled CPU counters for 5 seconds.
- Setup time ends when every TUI process is alive. TUI spawn check time uses a
  150 ms health-check floor, so it is useful only as a same-run comparison.

| Panes | Topology | App-server startup ms | Setup ms | TUI spawn check ms | App-server processes | App-server RSS MiB | TUI processes | TUI RSS MiB | Idle CPU % |
| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | per pane | 121.9 | 1416.7 | 153.6 | 1 | 321.8 | 1 | 188.2 | 10.79 |
| 1 | shared | 119.0 | 1417.4 | 153.9 | 1 | 335.6 | 1 | 193.3 | 11.19 |
| 5 | per pane | 125.0 | 7217.5 | 154.0 | 5 | 1638.5 | 5 | 1061.5 | 12.80 |
| 5 | shared | 118.9 | 6618.1 | 154.3 | 1 | 692.1 | 5 | 1062.0 | 43.16 |
| 10 | per pane | 116.1 | 14357.4 | 153.9 | 10 | 3166.4 | 10 | 1822.4 | 15.18 |
| 10 | shared | 118.8 | 13294.7 | 154.1 | 1 | 1049.0 | 10 | 2214.2 | 0.40 |

The comparable result is process count and RSS. At ten panes, sharing removed
nine app-server processes and reduced measured app-server RSS by 2117.4 MiB.
Total measured RSS was 4988.8 MiB per pane and 3263.2 MiB shared, a reduction
of 1725.6 MiB in this run. Setup was 1062.7 ms faster. The one-pane results are
effectively equal, as expected.

Idle CPU was noisy and non-monotonic even after the settle interval. It is raw
data, not evidence of an idle CPU improvement. A longer repeated benchmark is
the trigger for making a CPU claim.
