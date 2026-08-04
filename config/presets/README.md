# Use-case config presets

Fifteen complete, deployable `AppConfig` files — each a real Industry 4.0
or target-customer-vertical scenario built entirely on features this
firmware ships **today** (camera + AI + Phase 16 detection rules, PTZ,
SNMP, cluster mesh + cross-device fusion, correlation, overlays/widgets,
audio, storage/export). Every file here is covered by
`config::tests::every_shipped_preset_parses_and_validates`
(`src/config.rs`) — CI fails if a preset stops parsing or fails
`config::validate()`, so these can't silently bit-rot as the schema
evolves.

Two groups: the first 10 are single-camera Industry 4.0 scenarios; the
`cluster-fusion-*` files are two-or-more-camera deployments built around
this firmware's broker-less cross-device detection fusion
(`cluster/fusion.rs`) — one per target-customer vertical beyond pure
factory-floor Industry 4.0 (retail, critical infrastructure, campus,
construction, parking).

These are **not** the original TODO.md Phase 14e preset list — that list
(`safety-zone-guarding.toml`, `silo-level.toml`, `gate-counting.toml`,
`forklift-safety.toml`) is LiDAR-based, and LiDAR (Phase 14a-14d) isn't
built yet. Presets referencing a `[lidar]` section that doesn't exist
wouldn't actually be "production ready," so this set is the honest,
camera+AI-only equivalent — see each file's own header comment for what's
buildable today vs. what's flagged as a future LiDAR-fusion upgrade.

## How to use one

There's no `--config` flag — `src/main.rs` reads a fixed path:

```bash
cp config/presets/<preset>.toml config/iiotedge_default.toml
```

Then treat it exactly like the default file: run `make ci` if you've
edited it, `make run` to try it against the mock camera, `make deploy` to
push it to a device. **Before deploying to a real device**, every preset
still ships the same `CHANGE-ME` placeholders
(`[security].command_token`, `.api_token`, the default user's password)
as `config/iiotedge_default.toml` — generate a unique value per device
(`openssl rand -hex 24`), same warning as the main README.

Each preset also assumes the reference hardware profile (Radxa Zero 3E,
1920x1080, `yolov8n.rknn`) — adjust `[camera]`/`[ai].runtime`/
`[ai].model_path` for your actual device the same way you would for
`config/iiotedge_default.toml`.

## The presets

| File | Scenario | Showcases |
|---|---|---|
| `perimeter-intrusion-detection.toml` | Outdoor fence-line / yard boundary | `line_cross` tripwire + `presence` polygon, per-rule schedule, PTZ, SNMP |
| `restricted-machine-safety-zone.toml` | Person near a running press/robot cell | High-confidence `presence` rule, `gpio_output` warning relay, PLC fault correlation |
| `loading-dock-dwell-loitering.toml` | Dock-door loitering + unattended vehicles | Two `loiter` rules (short dwell for people, long dwell for vehicles), `cluster_broadcast` |
| `gate-entry-exit-counting.toml` | Vehicle + pedestrian gate counting | Two `line_cross` rules, `direction = "either"`, webhook-driven external tally, SNMP |
| `production-line-correlation-hmi.toml` | Conveyor/inspection station | AI **off** on purpose, machine-data correlation, sparkline/gauge/value widgets, audio anomaly listening |
| `multi-camera-cluster-warehouse.toml` | Multi-aisle warehouse coverage | Cluster mesh, cross-device detection fusion, `cluster.reactions` (tamper → peer snapshot, AI event → peer reanalyze) |
| `after-hours-facility-lockdown.toml` | Office/facility overnight security | Global `[schedule]` + independent per-rule schedule layered together, PTZ, SNMP trap, FTP nightly export |
| `ppe-ansi-compliance-zone.toml` | PPE-required work area | Raised `min_confidence`, `gpio_output` beacon, extended clip retention for audit review |
| `cold-storage-tamper-door-monitoring.toml` | Freezer / cold-chain room | AI **off** on purpose, tamper thresholds retuned for low light, motion zone + door-sensor correlation |
| `forklift-pedestrian-shared-lane.toml` | Shared forklift/pedestrian warehouse lane | Two `presence` rules on the same zone (person vs. vehicle), explicit caveat on what needs Phase 14 LiDAR fusion to do for real |
| `cluster-fusion-retail-loss-prevention.toml` | Retail store entrance + high-value aisle | Cross-device fusion, entrance detection → peer `reanalyze` reaction |
| `cluster-fusion-critical-infrastructure-perimeter.toml` | Utility/substation fence line + equipment yard | Fusion + cross-device tamper confirmation, SNMP trap, stricter mesh security posture |
| `cluster-fusion-campus-security.toml` | Education/corporate/healthcare campus | Fusion across building entrance/parking/walkway nodes, after-hours per-rule schedule, PTZ |
| `cluster-fusion-construction-site.toml` | Temporary jobsite perimeter + equipment yard | Fusion with no reliable uplink assumed, SD/USB auto-mirror as primary evidence path, equipment-dwell loiter rule |
| `cluster-fusion-smart-parking.toml` | Multi-level garage / lot entry + interior lanes | Fusion tracks a vehicle's lane handoff, no `cluster.reactions` (counting doesn't need cross-device commands) |

## Design notes

- **Full COCO-80 `ai.labels` kept intact everywhere AI is on.** The
  parser resolves a detection's class name by *index* into this list
  (`src/ai/parser.rs`) — trimming it to "just the classes I use" breaks
  the position alignment and silently mislabels detections. Every AI-
  enabled preset here copies the same full, correctly-ordered list from
  `config/iiotedge_default.toml`; only `class_filter` and each rule's
  `classes` narrow what's actually acted on.
- **Two presets turn AI off entirely** (`production-line-correlation-hmi`,
  `cold-storage-tamper-door-monitoring`) — a deliberate choice, not an
  oversight, for scenarios where running inference would cost compute
  without buying anything (upstream inspection already exists; low light
  tanks accuracy). Tamper/motion/correlation/widgets all work identically
  whether or not AI is enabled.
- **No preset claims a capability that doesn't exist.** Where a scenario's
  *real* ask needs something unbuilt (PTZ auto-follow on a rule match,
  true LiDAR-based proximity/speed, PPE-classifier detection with a stock
  COCO model), the header comment says so explicitly rather than
  configuring something that would silently no-op or mislead.
