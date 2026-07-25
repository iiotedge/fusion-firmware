# Benchmarks

Production baselines captured by `tests/benchmark.sh`. Mock-camera rows are a
**relative** regression baseline on a dev host; on-device rows (from
`tests/live-hw.sh`) are the real per-board figures. Newest first.

| date (UTC) | git | host | ready ms | 1st-frame ms | fps | RSS MB | CPU % | thr/fd | frames cap/drop | finalize ms |
|---|---|---|---|---|---|---|---|---|---|---|
| 2026-07-19T06:43:45Z | 7bcf286 | Darwin-arm64 | 2085 | 1282 | 29 | 67 | 4.5 | 39/79 | 860/0 | 1037 |
