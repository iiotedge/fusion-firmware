# QR Device Onboarding — Mobile App Integration Contract

Firmware-side spec for Phase 12a. This document is the contract the mobile app
scanner is built against — **no mobile-app-repo code was written or changed as
part of this work**; that's the app team's own implementation, using this doc.

Camera firmware source: `src/onboarding.rs`, routes in `src/core/metrics.rs`.
See also `docs/FEATURES.md` (Phase 12a) and `TODO.md` (Phase 12a) for the
as-built changelog.

## Why

Today, adding a camera to the app means typing in its LAN IP, ports, and RTSP/
ONVIF credentials by hand. QR onboarding replaces that: point the phone's
camera at a QR code the firmware generates, and the app gets everything it
needs — device identity, address, ports, credentials, and an API token — in
one scan.

## The two endpoints

Both are served on the existing metrics HTTP port (`[system].metrics_port`,
default `9100`), alongside `/metrics`, `/healthz` and `/cluster/status`.

| Route | Returns |
|---|---|
| `GET /onboarding/qr.png` | A PNG image — a scannable QR code encoding the JSON payload below |
| `GET /onboarding/info` | The identical payload as plain JSON (useful for a "manual add" fallback, or for testing without a camera) |

### Authentication (fetching the QR — not the same secret it hands out)

Both routes are gated by **`[security].command_token`** — the same
installer/provisioning secret already used for the MQTT/cluster command
channel. It is **not** the token embedded in the QR payload (see below); it
only controls who is allowed to *view* the QR/JSON in the first place.

Present it either way:

- `Authorization: Bearer <command_token>` header, **or**
- `?token=<command_token>` query parameter — needed because a QR code
  displayed via `<img src="http://camera:9100/onboarding/qr.png?token=...">`
  or a browser address bar can't set a custom header.

If `command_token` is empty on the device, both routes are open (matches
every other auth knob in this firmware: empty = unauthenticated, only
sensible on a trusted private LAN — the firmware logs a boot warning in this
case). If `[onboarding].enabled = false`, both routes return `404`.

A wrong/missing token returns `401`:
```json
{"ok": false, "error": "unauthorized: missing or invalid token"}
```

**Practical flow**: an installer (who already has `command_token` from the
provisioning sheet/fleet database) opens
`http://<camera-ip>:9100/onboarding/qr.png?token=<command_token>` on a laptop
or tablet during setup and lets the end user scan that screen with the app —
the end user's phone never needs to know `command_token`.

## Payload schema (`iiotedge.onboarding.v1`)

```json
{
  "schema": "iiotedge.onboarding.v1",
  "device_id": "cam-example-01",
  "facility_id": "site-1",
  "manufacturer": "IIoTEdge",
  "model": "Fusion Vision Node",
  "firmware_version": "1.0.0",
  "host": "192.0.2.10",
  "ports": {
    "onvif": 8000,
    "rtsp": 8554,
    "metrics": 9100
  },
  "rtsp": {
    "path": "/live",
    "auth_required": true,
    "username": "example-user",
    "password": "example-pass",
    "url": "rtsp://example-user:example-pass@192.0.2.10:8554/live"
  },
  "onvif": {
    "xaddr": "http://192.0.2.10:8000/onvif/device_service",
    "auth_required": true,
    "username": "example-user",
    "password": "example-pass"
  },
  "api_token": "example-api-token-generated-per-device",
  "group_id": "iiotedge",
  "node_id": "cam-example-01",
  "mqtt": {
    "host": "mqtt.example.com",
    "port": 1883,
    "tls_enabled": false
  }
}
```

**`group_id`/`node_id`/`mqtt` were added after a real hardware deployment
surfaced the gap** (2026-07-25): they are read from a *separate* config
document (`edge.toml`'s `EdgeConfig`, the iiotedge-lib telemetry config) —
NOT the same source as `device_id`/`facility_id` above (this firmware's own
`[system]` config). Critically, **`mqtt.host` is commonly a different host
entirely from `host` above** — on that real deployment the camera was at
`192.0.2.10` but its MQTT broker was a separate cloud host entirely. Do not
assume the broker lives on the camera's own IP; use `mqtt.host`
verbatim, and treat `mqtt: null` as "not available from this QR" (telemetry
off, or a non-MQTT northbound transport), not as "assume some default".

### Field reference

| Field | Type | Notes |
|---|---|---|
| `schema` | string | Always `"iiotedge.onboarding.v1"` today. **Reject/warn on any value you don't recognize** rather than guessing field meanings — this is how future incompatible changes will be signaled. |
| `device_id` | string | Stable identity — use as the primary key for the device in the app's local store; also what the cluster/fusion feature (`/cluster/status`) reports peers by. |
| `facility_id` | string | Site/group label, useful for grouping devices in the UI. |
| `manufacturer`, `model` | string | Display fields, same values ONVIF `GetDeviceInformation` reports. |
| `firmware_version` | string | Semver-ish, for display/compatibility checks. |
| `host` | string | The LAN IP the camera is reachable on. Derived from the HTTP `Host` header of the request that fetched this payload — correct even on multi-homed devices, not a best-effort guess. |
| `ports.onvif` | u16 | ONVIF SOAP device/media service port. |
| `ports.rtsp` | u16 | RTSP port. |
| `ports.metrics` | u16 | This same HTTP API's port (`/cluster/status`, `/healthz`, etc). |

> These three are always the *reachable* ports — trust them as-is. Most
> devices report their real listen port directly, but a device behind
> port-forwarding/NAT (`[onboarding].external_*_port` in its config — see
> `iiotedge-cluster-sim`'s docker-compose setup for a worked example, where
> several simulated nodes share one host and need distinct external ports)
> reports the externally-dialable port instead. The app never needs to know
> which case it's in.
| `rtsp.path` | string | RTSP mount path, e.g. `/live`. |
| `rtsp.auth_required` | bool | `false` when `[security].users` is empty on the device (anonymous RTSP). |
| `rtsp.username`/`password` | string \| null | `null` when `auth_required` is `false`. |
| `rtsp.url` | string | Fully composed, ready to hand to any RTSP player. |
| `onvif.xaddr` | string | ONVIF device service SOAP endpoint. |
| `onvif.auth_required`, `.username`, `.password` | — | Same shared credential store as RTSP (WS-UsernameToken auth) — today these are always identical to the `rtsp` fields, kept separate in the schema so they can diverge later without a version bump. |
| `api_token` | string | **Store this securely (Keychain/Keystore, not plain prefs/logs).** Present it as `Authorization: Bearer <api_token>` on `/cluster/status` and any future authenticated HTTP route this firmware adds. Empty string means that API is unauthenticated on this particular device. |
| `group_id`, `node_id` | string | Sparkplug group/node identity from `edge.toml`'s `[node]` — **not** the same values as `facility_id`/`device_id` above (different config document). Empty strings when `edge.toml` couldn't be read. This is what MQTT topics are actually built from (`iiotedge/{group_id}/{node_id}/...`); don't substitute `facility_id`/`device_id` for these even though they're often superficially similar-looking strings in test setups. |
| `mqtt` | object \| null | The real MQTT broker this device's telemetry/command channel connects to. `null` when telemetry is off, `edge.toml` was unreadable, or the configured northbound transport isn't `"mqtt"` — treat that as "not available", don't guess a fallback (in particular, **never default `mqtt.host` to the `host` field above** — see the worked real-deployment example further up). |
| `mqtt.host`, `mqtt.port` | string, u16 | Broker address. Frequently a separate cloud/on-prem host, not the camera itself. |
| `mqtt.tls_enabled` | bool | Whether `edge.toml`'s `[northbound.tls]` is on for this connection. |

## Building the scanner (app side)

1. **Scan**: use your platform's standard QR/barcode reader (e.g. iOS
   `AVFoundation`/`VisionKit`, Android `ML Kit` or `ZXing`, or your
   cross-platform framework's camera-scanning plugin) pointed at
   `/onboarding/qr.png`. No custom QR format — it's a standard QR code
   whose payload is exactly the UTF-8 JSON above.
2. **Parse & validate**: `JSON.parse` the scanned text, check
   `schema == "iiotedge.onboarding.v1"` before trusting any other field.
3. **Persist**: save `device_id`, `host`, `ports`, `rtsp`, `onvif` into your
   device list as you would for a manually-added camera. Save `api_token`
   into secure storage, keyed by `device_id`.
4. **Connect**:
   - Video: open `rtsp.url` directly in your player.
   - ONVIF calls (if you use them for PTZ/profile discovery): POST SOAP to
     `onvif.xaddr` with a WS-UsernameToken built from `onvif.username`/`password`.
   - Status/fusion API: `GET http://<host>:<ports.metrics>/cluster/status`
     with header `Authorization: Bearer <api_token>` (omit the header
     entirely if `api_token` was an empty string).
5. **Re-scan is safe**: scanning the same device again just re-fetches the
   current payload — `host` may have changed (DHCP), credentials may have
   been rotated; treat a fresh scan as the source of truth and overwrite
   your stored copy for that `device_id`.

## Security notes for the app implementation

- **This is a LAN-only, unencrypted HTTP contract today** — no TLS on
  `/onboarding/*`, `/cluster/status`, RTSP, or ONVIF. Only fetch these
  endpoints on networks you trust (the same WiFi the camera is on), and
  don't proxy them over the public internet without adding your own TLS
  termination in front.
- `api_token` and the RTSP/ONVIF password are plaintext in the payload by
  design — QR onboarding's entire point is to hand over these values instead
  of requiring the user to type them in. Treat the whole payload as a secret
  once scanned: don't log it, don't send it anywhere except the device it
  came from.
- `command_token` (the secret that unlocks `/onboarding/*` itself) is
  **not** in the payload and the app never needs it — that stays with
  installers/provisioning tooling.
- If a phone is lost/compromised, rotate that device's `api_token` (and,
  if RTSP/ONVIF credentials were also exposed, those too) in the camera's
  config and re-provision — there's no per-app token revocation today,
  only per-device rotation.

## Testing without a phone

```sh
# JSON payload (replace <camera-ip> and <command_token>):
curl -s "http://<camera-ip>:9100/onboarding/info?token=<command_token>" | python3 -m json.tool

# QR image — open in a browser, or save and scan from a screen:
curl -s "http://<camera-ip>:9100/onboarding/qr.png?token=<command_token>" -o qr.png

# Using the new api_token against the now-gated status endpoint:
curl -s http://<camera-ip>:9100/cluster/status \
  -H "Authorization: Bearer <api_token>"
```

If you're using the `iiotedge-cluster-sim` test cluster (real firmware,
Docker, multiple nodes on one Mac — see that repo's own README) note its
default template config ships with empty `api_token`/`command_token`, so
those endpoints are open there unless you add them to
`docker/config/firmware.template.toml` yourself.
