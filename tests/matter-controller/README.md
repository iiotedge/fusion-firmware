# Real-controller Matter verification

Commissions a freshly booted `fusion-firmware` with **matter.js** — an
independent Matter implementation — and exercises each endpoint through the
Matter Interaction Model (reads, writes, commands, change reports).

```
cargo build
tests/matter-controller/run.sh          # needs node >= 20; installs matter.js on first run
```

What it checks today: commissioning over IP (PASE + CASE), endpoint/device-type
structure, OnOff relay commands, color-light Level/Color commands, and the
Thermostat's attribute writes, `SetpointRaiseLower`, constraint rejection and
subscription change reports.

Notes
- Reads use `requestFromRemote = true`. matter.js otherwise answers from its
  subscription cache, which makes a stale value look like a firmware bug.
- Change *reporting* is checked separately (polling the controller's local cache
  after a command) — that is what Apple Home's UI relies on.
- `LocalTemperature` is `null` on hosts without `/sys/class/thermal` (e.g. macOS):
  the Thermostat reports "no reading" rather than inventing one.
- Commissioning uses a known IP address, so no mDNS is needed. The firmware uses
  rs-matter's test credentials (passcode 20202021, discriminator 3840).
