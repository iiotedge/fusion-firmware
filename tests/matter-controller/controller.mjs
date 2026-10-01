// Real Matter controller harness (matter.js) for fusion-firmware.
//
// Commissions the firmware over IP (no mDNS needed: known address), dumps its
// endpoint structure, and exercises real reads / writes / commands through
// the Matter Interaction Model — the only way to verify the hand-written
// clusters and the router against an independent implementation.
//
// usage: node controller.mjs --ip 127.0.0.1 --port 5540 [--passcode 20202021]
//        [--discriminator 3840] [--storage /tmp/dir] [--keep]
import "@matter/nodejs";
import { Environment } from "@matter/main";
import { CommissioningController } from "@project-chip/matter.js";
import {
    OnOff, LevelControl, ColorControl, Thermostat, Descriptor, BasicInformation,
    GeneralCommissioning, TemperatureMeasurement, RelativeHumidityMeasurement,
    PressureMeasurement, IlluminanceMeasurement, FlowMeasurement, OccupancySensing,
    BooleanState, Identify, FanControl, Switch, SoilMeasurement, AirQuality,
    CarbonDioxideConcentrationMeasurement, Pm25ConcentrationMeasurement,
    Pm10ConcentrationMeasurement, TotalVolatileOrganicCompoundsConcentrationMeasurement,
} from "@matter/main/clusters";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const args = Object.fromEntries(
    process.argv.slice(2).reduce((acc, a, i, all) => {
        if (a.startsWith("--")) acc.push([a.slice(2), all[i + 1] && !all[i + 1].startsWith("--") ? all[i + 1] : true]);
        return acc;
    }, []),
);
const ip = args.ip ?? "127.0.0.1";
const port = Number(args.port ?? 5540);
const passcode = Number(args.passcode ?? 20202021);
const discriminator = Number(args.discriminator ?? 3840);
const storage = args.storage ?? fs.mkdtempSync(path.join(os.tmpdir(), "fusion-mjs-"));
const metricsPort = Number(args["metrics-port"] ?? 9100);
const token = args.token ?? "fusion-verify-token";

// Feed a value into a `push:<name>` signal over the firmware's HTTP API.
async function push(name, value) {
    const r = await fetch(`http://127.0.0.1:${metricsPort}/signals/${name}`, {
        method: "POST",
        headers: { Authorization: `Bearer ${token}`, "Content-Type": "application/json" },
        body: JSON.stringify({ value }),
    });
    return r.status;
}

const results = [];
function check(name, ok, detail = "") {
    results.push({ name, ok, detail });
    console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? "  — " + detail : ""}`);
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const show = (v) => JSON.stringify(v, (_k, x) => (typeof x === "bigint" ? Number(x) : x));

const environment = Environment.default;
environment.vars.set("path.root", storage);

const controller = new CommissioningController({
    environment: { environment, id: "fusion-verify" },
    autoConnect: false,
    adminFabricLabel: "fusion-verify",
});
await controller.start();

console.log(`commissioning ${ip}:${port} passcode=${passcode} discriminator=${discriminator} (storage ${storage})`);
let nodeId;
try {
    nodeId = await controller.commissionNode({
        commissioning: {
            regulatoryLocation: GeneralCommissioning.RegulatoryLocationType.IndoorOutdoor,
            regulatoryCountryCode: "XX",
        },
        discovery: {
            knownAddress: { ip, port, type: "udp" },
            identifierData: { longDiscriminator: discriminator },
            timeout: 30,
        },
        passcode,
    });
} catch (e) {
    check("commission over IP (PASE + CASE)", false, String(e?.message ?? e));
    await controller.close();
    process.exit(2);
}
check("commission over IP (PASE + CASE)", true, `node id ${nodeId}`);

const node = await controller.getNode(nodeId);
// commissionNode() already awaited initialization; awaiting the event again
// would wait forever for an emit that already happened.
if (!node.initialized) await node.events.initialized;

// ---- structure -------------------------------------------------------------
const root = node.getRootClusterClient(BasicInformation);
const endpoints = node.getDevices();
const byNumber = new Map(endpoints.map((e) => [e.number, e]));
if (root) {
    const vendor = await root.getVendorNameAttribute(true);
    const product = await root.getProductNameAttribute(true);
    check("BasicInformation readable", true, `vendor=${vendor} product=${product}`);
    // Identity comes from config, with defaults derived from what the node exposes.
    check("BasicInformation: VendorName is the configured one", vendor === "Acme Controls", vendor);
    check("BasicInformation: ProductName is derived (a camera says so; a node without one does not)",
        product === (byNumber.has(1) ? "fusion-firmware Camera" : "fusion-firmware"), product);
    if (args["sw-version"]) {
        check("BasicInformation: SoftwareVersionString is this build's version",
            (await root.getSoftwareVersionStringAttribute(true)) === args["sw-version"],
            await root.getSoftwareVersionStringAttribute(true));
    }
}
console.log("endpoints:", [...byNumber.keys()].join(", "));
for (const ep of endpoints) {
    const desc = ep.getClusterClient(Descriptor);
    const types = desc ? (await desc.getDeviceTypeListAttribute(true)).map((d) => "0x" + Number(d.deviceType).toString(16)) : [];
    const servers = desc ? (await desc.getServerListAttribute(true)).map((c) => "0x" + Number(c).toString(16)) : [];
    let extra = "";
    if (desc) {
        try {
            const tags = await desc.getTagListAttribute?.();
            if (tags) extra += ` tags=${show(tags)}`;
        } catch { /* attribute not advertised */ }
        try {
            const uid = await desc.getEndpointUniqueIdAttribute?.();
            if (uid) extra += ` uniqueId=${uid}`;
        } catch { /* attribute not advertised */ }
    }
    console.log(`  endpoint ${ep.number}: types=[${types}] servers=[${servers}]${extra}`);
}

// Poll a LOCAL (subscription-cache) read until it matches, to prove the device
// pushed the change to a subscribed controller (not just that a re-read works).
async function reported(read, expected, ms = 4000) {
    const end = Date.now() + ms;
    for (;;) {
        const v = await read();
        if (v === expected) return true;
        if (Date.now() > end) return false;
        await new Promise((r) => setTimeout(r, 100));
    }
}
const expect = (list) => list.forEach(([n, ok, d]) => check(n, ok, d));

// Events, the way a real controller sees them: DELIVERED over its subscription
// (reading the log back is not the same thing — matter.js's own getters skip
// events the subscription already handed over). Each event once, by number.
const eventLog = [];
const eventSeen = new Set();
const eventConsumed = {};
function listenForEvents(ep, client, names) {
    for (const name of names) {
        client[`add${name}EventListener`]?.((e) => {
            const n = Number(e.eventNumber);
            if (eventSeen.has(n)) return;
            eventSeen.add(n);
            eventLog.push({ ep, name, n, data: e.data });
        });
    }
}
const eventsOf = (ep) => eventLog.filter((e) => e.ep === ep).sort((a, b) => a.n - b.n);
// Skip everything delivered so far (for endpoints whose earlier events we don't assert on).
const drainEvents = (ep) => { eventConsumed[ep] = eventsOf(ep).length; };
// Wait for `count` further events from `ep`, then a short quiet period so a stray
// extra event shows up as a failure.
async function nextEvents(ep, count, timeoutMs = 4000) {
    const end = Date.now() + timeoutMs;
    const fresh = () => eventsOf(ep).slice(eventConsumed[ep] ?? 0);
    while (fresh().length < count && Date.now() < end) await sleep(50);
    if (count > 0) await sleep(300);
    const got = fresh();
    eventConsumed[ep] = (eventConsumed[ep] ?? 0) + got.length;
    return got;
}

// ---- On/Off relay (endpoint 2) --------------------------------------------
if (byNumber.has(2)) {
    const c = byNumber.get(2).getClusterClient(OnOff);
    await c.on();
    const a = await c.getOnOffAttribute(true);
    await c.off();
    const b = await c.getOnOffAttribute(true);
    await c.toggle();
    const t = await c.getOnOffAttribute(true);
    check("ep2 OnOff: change REPORTED to subscribed controller",
        await reported(() => c.getOnOffAttribute(false), true));
    expect([
        ["ep2 OnOff: on() -> true", a === true, `got ${a}`],
        ["ep2 OnOff: off() -> false", b === false, `got ${b}`],
        ["ep2 OnOff: toggle() -> true", t === true, `got ${t}`],
    ]);
    await c.off();
}

// ---- Extended color light (endpoint 3) ------------------------------------
if (byNumber.has(3)) {
    const ep = byNumber.get(3);
    const onoff = ep.getClusterClient(OnOff);
    const level = ep.getClusterClient(LevelControl);
    const color = ep.getClusterClient(ColorControl);
    await onoff.on();
    check("ep3 OnOff: on() -> true", (await onoff.getOnOffAttribute(true)) === true);
    await level.moveToLevel({ level: 100, transitionTime: 0, optionsMask: {}, optionsOverride: {} });
    const lv = await level.getCurrentLevelAttribute(true);
    check("ep3 LevelControl: moveToLevel(100)", lv === 100, `CurrentLevel=${lv}`);
    await color.moveToHueAndSaturation({
        hue: 120, saturation: 200, transitionTime: 0, optionsMask: {}, optionsOverride: {},
    });
    const hue = await color.getCurrentHueAttribute(true);
    const sat = await color.getCurrentSaturationAttribute(true);
    check("ep3 ColorControl: moveToHueAndSaturation(120,200)", hue === 120 && sat === 200, `hue=${hue} sat=${sat}`);
    await onoff.off();
}

// ---- Thermostat (endpoint 4) ----------------------------------------------
if (byNumber.has(4)) {
    const t = byNumber.get(4).getClusterClient(Thermostat);
    const local = await t.getLocalTemperatureAttribute(true);
    check("ep4 Thermostat: LocalTemperature readable (number or null)",
        local === null || typeof local === "number", `LocalTemperature=${local}`);
    await t.setOccupiedHeatingSetpointAttribute(2100);
    check("ep4 Thermostat: write OccupiedHeatingSetpoint=2100",
        (await t.getOccupiedHeatingSetpointAttribute(true)) === 2100);
    await t.setpointRaiseLower({ mode: Thermostat.SetpointRaiseLowerMode.Heat, amount: 5 });
    const after = await t.getOccupiedHeatingSetpointAttribute(true);
    check("ep4 Thermostat: SetpointRaiseLower(Heat,+0.5C)", after === 2150, `setpoint=${after}`);
    check("ep4 Thermostat: setpoint change REPORTED to subscribed controller",
        await reported(() => t.getOccupiedHeatingSetpointAttribute(false), 2150));
    await t.setSystemModeAttribute(Thermostat.SystemMode.Heat);
    check("ep4 Thermostat: SystemMode=Heat",
        (await t.getSystemModeAttribute(true)) === Thermostat.SystemMode.Heat);
    let rejected = false;
    try { await t.setSystemModeAttribute(99); } catch { rejected = true; }
    check("ep4 Thermostat: out-of-range SystemMode rejected", rejected);
}

// The spec makes Identify mandatory on the On/Off Light, Extended Color Light
// and Thermostat device types behind endpoints 2, 3 and 4.
for (const n of [2, 3, 4]) {
    if (!byNumber.has(n)) continue;
    const c = byNumber.get(n).getClusterClient(Identify);
    let ok = false;
    try { ok = (await c.getIdentifyTypeAttribute(true)) !== undefined; } catch { /* missing */ }
    check(`ep${n}: mandatory Identify cluster present`, ok);
}

// ---- Config-driven sensors ([[matter.endpoints]], endpoints.toml) -----------
if (byNumber.has(20)) {
    // [endpoint, signal, cluster, label, pushed value, expected raw MeasuredValue]
    const measured = [
        [20, "boiler_temp", TemperatureMeasurement, "temperature 21.5C", 21.5, 2150],
        [21, "room_rh", RelativeHumidityMeasurement, "humidity 45.5%", 45.5, 4550],
        [22, "hpa", PressureMeasurement, "pressure 1013 hPa", 1013, 1013],
        [23, "lux", IlluminanceMeasurement, "illuminance 100 lux", 100, 20001],
        [24, "flow", FlowMeasurement, "flow 12.3 m3/h", 12.3, 123],
        [27, "outlet_temp", TemperatureMeasurement, "temperature 60.25C", 60.25, 6025],
    ];
    for (const [n, signal, cluster, label, value, raw] of measured) {
        const c = byNumber.get(n).getClusterClient(cluster);
        const before = await c.getMeasuredValueAttribute(true);
        check(`ep${n} ${label}: null before anything is pushed (never a made-up 0)`, before === null, `got ${before}`);
        const st = await push(signal, value);
        check(`ep${n} ${label}: HTTP push accepted`, st === 200, `status ${st}`);
        const after = await c.getMeasuredValueAttribute(true);
        check(`ep${n} ${label}: MeasuredValue over Matter == ${raw}`, after === raw, `got ${after}`);
        check(`ep${n} ${label}: change REPORTED to subscribed controller`,
            await reported(() => c.getMeasuredValueAttribute(false), raw));
        const min = await c.getMinMeasuredValueAttribute(true);
        const max = await c.getMaxMeasuredValueAttribute(true);
        check(`ep${n} ${label}: Min/MaxMeasuredValue present`, min !== null && max !== null && min < max, `min=${min} max=${max}`);
    }
    // out-of-range reading must not be trusted
    {
        const c = byNumber.get(20).getClusterClient(TemperatureMeasurement);
        await push("boiler_temp", 900);
        const v = await c.getMeasuredValueAttribute(true);
        check("ep20 temperature: out-of-range push (900C) reads as null", v === null, `got ${v}`);
    }
    // push validation
    check("HTTP push rejects a non-numeric value", (await push("boiler_temp", "hot")) === 422);

    // Occupancy (radar)
    {
        const c = byNumber.get(25).getClusterClient(OccupancySensing);
        // matter.js surfaces a per-attribute error status as `undefined` (it may
        // also throw); either way the controller sees "no value", not "unoccupied".
        const noValue = await c.getOccupancyAttribute(true).catch(() => undefined);
        const unreadable = noValue === undefined || noValue === null;
        check("ep25 occupancy: no reading -> attribute unavailable, not 'unoccupied'", unreadable);
        await push("presence", true);
        const occ = await c.getOccupancyAttribute(true);
        check("ep25 occupancy: pushed true -> occupied", occ?.occupied === true, show(occ));
        check("ep25 occupancy: change REPORTED to subscribed controller",
            await reported(async () => (await c.getOccupancyAttribute(false))?.occupied, true));
        await push("presence", false);
        check("ep25 occupancy: pushed false -> unoccupied", (await c.getOccupancyAttribute(true))?.occupied === false);
        const feats = await c.getFeatureMapAttribute(true);
        check("ep25 occupancy: advertises the RADAR technology feature (Matter 1.5)", feats?.radar === true, show(feats));
    }
    // Contact sensor (BooleanState)
    {
        const c = byNumber.get(26).getClusterClient(BooleanState);
        const noValue = await c.getStateValueAttribute(true).catch(() => undefined);
        const unreadable = noValue === undefined || noValue === null;
        check("ep26 contact: no reading -> attribute unavailable, not 'open'", unreadable);
        await push("door", true);
        check("ep26 contact: pushed true -> StateValue true", (await c.getStateValueAttribute(true)) === true);
        check("ep26 contact: change REPORTED to subscribed controller",
            await reported(() => c.getStateValueAttribute(false), true));
        await push("door", false);
        check("ep26 contact: pushed false -> StateValue false", (await c.getStateValueAttribute(true)) === false);
    }
    // Mandatory Identify cluster on every sensor
    for (const n of [20, 21, 22, 23, 24, 25, 26, 27]) {
        const c = byNumber.get(n).getClusterClient(Identify);
        let ok = false;
        try { ok = (await c.getIdentifyTypeAttribute(true)) !== undefined; } catch { /* missing */ }
        check(`ep${n}: mandatory Identify cluster present`, ok);
    }
    // Descriptor: shared device type => distinct, non-empty TagLists; dynamic => UniqueID
    {
        const tags = {};
        for (const n of [20, 27]) {
            const d = byNumber.get(n).getClusterClient(Descriptor);
            tags[n] = await d.getTagListAttribute(true);
        }
        const t20 = show(tags[20]);
        const t27 = show(tags[27]);
        check("ep20/ep27 share a device type: both carry a non-empty TagList",
            (tags[20]?.length ?? 0) > 0 && (tags[27]?.length ?? 0) > 0, `${t20} | ${t27}`);
        check("ep20/ep27: TagLists are distinct (Matter Core spec 9.5)", t20 !== t27);
        const uid = await byNumber.get(20).getClusterClient(Descriptor).getEndpointUniqueIdAttribute(true);
        check("ep20: dynamic endpoint advertises a stable UniqueID from its name", uid === "Boiler temp", `got ${uid}`);
        // unique device types stay untagged (no Descriptor change for them)
        const d21 = byNumber.get(21).getClusterClient(Descriptor);
        let humidityTagged = true;
        try { const t = await d21.getTagListAttribute(true); humidityTagged = (t?.length ?? 0) > 0; } catch { humidityTagged = false; }
        check("ep21: a device type that is unique on the node carries no TagList", humidityTagged === false);
    }
}


// ---- More detectors, soil moisture and change events ------------------------
if (byNumber.has(50)) {
    const deviceTypes = async (n) =>
        (await byNumber.get(n).getClusterClient(Descriptor).getDeviceTypeListAttribute(true)).map((d) => Number(d.deviceType));
    check("ep50 leak sensor: device type Water Leak Detector (0x43)", (await deviceTypes(50)).includes(0x43));
    check("ep51 rain sensor: device type Rain Sensor (0x44)", (await deviceTypes(51)).includes(0x44));
    check("ep52 freeze sensor: device type Water Freeze Detector (0x41)", (await deviceTypes(52)).includes(0x41));
    check("ep53 soil sensor: device type Soil Sensor (0x45)", (await deviceTypes(53)).includes(0x45));

    // Boolean "detected" sensors: true = detected, with the StateChange event.
    for (const [n, signal, what] of [[50, "leak", "leak"], [51, "rain", "rain"], [52, "freeze", "freeze"]]) {
        const c = byNumber.get(n).getClusterClient(BooleanState);
        listenForEvents(n, c, ["StateChange"]);
        const before = await c.getStateValueAttribute(true).catch(() => undefined);
        check(`ep${n} ${what}: no reading -> StateValue unavailable, not 'dry/clear'`, before === undefined || before === null);
        check(`ep${n} ${what}: advertises the CHANGE_EVENT feature`,
            (await c.getFeatureMapAttribute(true))?.changeEvent === true);
        await push(signal, true);
        check(`ep${n} ${what}: detected -> StateValue true`, (await c.getStateValueAttribute(true)) === true);
        check(`ep${n} ${what}: change REPORTED to subscribed controller`,
            await reported(() => c.getStateValueAttribute(false), true));
        let got = await nextEvents(n, 1);
        check(`ep${n} ${what}: StateChange(true) event DELIVERED to the subscribed controller`,
            got.length === 1 && got[0].name === "StateChange" && got[0].data?.stateValue === true, show(got.map((e) => e.data)));
        await push(signal, false);
        got = await nextEvents(n, 1);
        check(`ep${n} ${what}: cleared -> StateChange(false) event`,
            got.length === 1 && got[0].data?.stateValue === false, show(got.map((e) => e.data)));
        check(`ep${n} ${what}: mandatory Identify cluster present`,
            (await byNumber.get(n).getClusterClient(Identify).getIdentifyTypeAttribute(true).catch(() => undefined)) !== undefined);
    }

    // The existing contact + radar occupancy sensors gain their change events too.
    {
        const door = byNumber.get(26).getClusterClient(BooleanState);
        listenForEvents(26, door, ["StateChange"]);
        await push("door", true);
        let got = await nextEvents(26, 1);
        check("ep26 contact: StateChange(true) event DELIVERED",
            got.length === 1 && got[0].data?.stateValue === true, show(got.map((e) => e.data)));
        await push("door", false);
        await nextEvents(26, 1);

        const radar = byNumber.get(25).getClusterClient(OccupancySensing);
        listenForEvents(25, radar, ["OccupancyChanged"]);
        check("ep25 occupancy: advertises the OCCUPANCY_EVENT feature (Matter 1.5, cluster rev 7)",
            (await radar.getFeatureMapAttribute(true))?.occupancyEvent === true);
        drainEvents(25);
        await push("presence", true);
        got = await nextEvents(25, 1);
        check("ep25 occupancy: OccupancyChanged(occupied) event DELIVERED",
            got.length === 1 && got[0].name === "OccupancyChanged" && got[0].data?.occupancy?.occupied === true, show(got.map((e) => e.data)));
        await push("presence", false);
        got = await nextEvents(25, 1);
        check("ep25 occupancy: OccupancyChanged(empty) event DELIVERED",
            got.length === 1 && got[0].data?.occupancy?.occupied === false, show(got.map((e) => e.data)));
    }

    // Soil moisture (Matter 1.5).
    {
        const c = byNumber.get(53).getClusterClient(SoilMeasurement);
        const before = await c.getSoilMoistureMeasuredValueAttribute(true);
        check("ep53 soil: null before anything is pushed (never a made-up 0)", before === null, `got ${before}`);
        await push("soil", 42.4);
        const v = await c.getSoilMoistureMeasuredValueAttribute(true);
        check("ep53 soil: pushed 42.4% -> SoilMoisture 42", v === 42, `got ${v}`);
        check("ep53 soil: change REPORTED to subscribed controller",
            await reported(() => c.getSoilMoistureMeasuredValueAttribute(false), 42));
        const limits = await c.getSoilMoistureMeasurementLimitsAttribute(true);
        check("ep53 soil: MeasurementLimits describe a 0-100 soil-moisture range with one accuracy range",
            limits?.measurementType === 17 && limits?.measured === true
            && Number(limits?.minMeasuredValue) === 0 && Number(limits?.maxMeasuredValue) === 100
            && limits?.accuracyRanges?.length === 1, show(limits));
        await push("soil", 150);
        const bad = await c.getSoilMoistureMeasuredValueAttribute(true);
        check("ep53 soil: out-of-range push (150%) reads as null", bad === null, `got ${bad}`);
        check("ep53 soil: mandatory Identify cluster present",
            (await byNumber.get(53).getClusterClient(Identify).getIdentifyTypeAttribute(true).catch(() => undefined)) !== undefined);
    }
}

// ---- [[tags]]: the firmware's REAL Modbus driver -> signals -> Matter -----------
// run.sh runs a Modbus TCP simulator and points the SDK's Modbus driver at it, so
// this proves southbound machine data reaches a Matter controller with no glue
// script: register bytes -> [[tags]] decoding -> a signal -> a Matter endpoint.
if (byNumber.has(80)) {
    const simPort = Number(args["sim-port"] ?? 5021);
    const sim = (path, body = {}) =>
        fetch(`http://127.0.0.1:${simPort}${path}`, { method: "POST", body: JSON.stringify(body) }).then((r) => r.text());
    const temp = byNumber.get(80).getClusterClient(TemperatureMeasurement);
    const pressure = byNumber.get(81).getClusterClient(PressureMeasurement);
    const valve = byNumber.get(82).getClusterClient(BooleanState);

    // i16 register * 0.1: 215 -> 21.5 C -> 2150 (0.01 C).
    await sim("/set", { holding: { 0: [215] } });
    check("Modbus -> Matter: holding register 215 (i16, scale 0.1) becomes 21.5 C on a temperature endpoint",
        await reported(() => temp.getMeasuredValueAttribute(true), 2150, 8000));

    // f32 across two registers, low register first ("CDAB"): 1008.4 hPa -> 1008.
    // The tag has `offset = 2` — a BYTE offset, i.e. register 1 (registers 1-2).
    {
        const b = Buffer.alloc(4);
        b.writeFloatBE(1008.4);
        const hi = b.readUInt16BE(0);
        const lo = b.readUInt16BE(2);
        await sim("/set", { holding: { 1: [lo, hi] } });
        check("Modbus -> Matter: an f32 spanning two registers with word_order = swap becomes 1008 hPa",
            await reported(() => pressure.getMeasuredValueAttribute(true), 1008, 8000));
    }

    // A coil read, one byte per point: coil 1 -> bool at offset 1.
    await sim("/set", { coils: { 1: [1] } });
    check("Modbus -> Matter: coil 1 (bool at offset 1) becomes a contact sensor's StateValue true",
        await reported(() => valve.getStateValueAttribute(true), true, 8000));
    await sim("/set", { coils: { 1: [0] } });
    check("Modbus -> Matter: ...and false again",
        await reported(() => valve.getStateValueAttribute(true), false, 8000));

    // The signal behind it is visible to anything else on the device.
    {
        const sig = await signalsNow();
        check("the tag's signal is on the bus (GET /signals): push:mb_boiler_temp = 21.5",
            Math.abs(sig["push:mb_boiler_temp"] - 21.5) < 1e-9, show(sig["push:mb_boiler_temp"]));
    }

    // A dead link must read as no data, not as a temperature that never changes.
    await sim("/pause");
    check("a DEAD Modbus link reads as no data (Matter null) once max_age_s passes — not the last temperature",
        await reported(() => temp.getMeasuredValueAttribute(true), null, 12000));
    await sim("/resume");
    check("the link comes back: the reading returns",
        await reported(() => temp.getMeasuredValueAttribute(true), 2150, 60000));
}

// ---- Air quality: one endpoint, several measurements -------------------------
if (byNumber.has(70)) {
    const ep = byNumber.get(70);
    const types = (await ep.getClusterClient(Descriptor).getDeviceTypeListAttribute(true)).map((d) => Number(d.deviceType));
    check("ep70 air quality: device type Air Quality Sensor (0x2c)", types.includes(0x2c), show(types));
    const servers = (await ep.getClusterClient(Descriptor).getServerListAttribute(true)).map(Number);
    check("ep70 air quality: ONE endpoint carries AirQuality + the configured CO2, PM2.5, TVOC, temperature, humidity + Identify",
        [0x5b, 0x40d, 0x42a, 0x42e, 0x402, 0x405, 0x03].every((c) => servers.includes(c)), show(servers.map((c) => "0x" + c.toString(16))));
    check("ep70 air quality: no concentration cluster that wasn't configured",
        ![0x40c, 0x413, 0x415, 0x42c, 0x42d, 0x42b, 0x42f].some((c) => servers.includes(c)));

    const aq = ep.getClusterClient(AirQuality);
    const co2 = ep.getClusterClient(CarbonDioxideConcentrationMeasurement);
    const pm25 = ep.getClusterClient(Pm25ConcentrationMeasurement);
    const tvoc = ep.getClusterClient(TotalVolatileOrganicCompoundsConcentrationMeasurement);
    const AQ = AirQuality.AirQualityEnum;
    const level = (remote = true) => aq.getAirQualityAttribute(remote);

    const feats = await aq.getFeatureMapAttribute(true);
    check("ep70 AirQuality: advertises Fair, Moderate, VeryPoor and ExtremelyPoor",
        feats?.fair && feats?.moderate && feats?.veryPoor && feats?.extremelyPoor, show(feats));
    check("ep70: nothing measured yet -> AirQuality is Unknown (not Good) and every concentration is null",
        (await level()) === AQ.Unknown
        && (await co2.getMeasuredValueAttribute(true)) === null
        && (await pm25.getMeasuredValueAttribute(true)) === null
        && (await tvoc.getMeasuredValueAttribute(true)) === null, `level=${await level()}`);
    // Units are the Matter units a source must be in: ppm / ug/m3 / ppb; medium air.
    check("ep70 units: CO2 ppm, PM2.5 ug/m3, TVOC ppb, all measured in air",
        (await co2.getMeasurementUnitAttribute(true)) === 0 && (await pm25.getMeasurementUnitAttribute(true)) === 4
        && (await tvoc.getMeasurementUnitAttribute(true)) === 1
        && (await co2.getMeasurementMediumAttribute(true)) === 0);
    {
        const lo = await co2.getMinMeasuredValueAttribute(true);
        const hi = await co2.getMaxMeasuredValueAttribute(true);
        check("ep70 CO2: Min/MaxMeasuredValue describe the sensor's range", lo === 0 && hi === 10000, `${lo}..${hi}`);
    }

    await push("co2", 450);
    check("ep70 CO2: 450 ppm over Matter", (await co2.getMeasuredValueAttribute(true)) === 450);
    check("ep70 AirQuality: CO2 450 ppm -> Good, REPORTED to subscribed controller",
        (await level()) === AQ.Good && (await reported(() => level(false), AQ.Good)));
    await push("pm25", 40);
    check("ep70 PM2.5: 40 ug/m3 over Matter", (await pm25.getMeasuredValueAttribute(true)) === 40);
    check("ep70 AirQuality: PM2.5 40 ug/m3 (EPA 'unhealthy for sensitive groups') drags the level to Moderate, REPORTED",
        (await level()) === AQ.Moderate && (await reported(() => level(false), AQ.Moderate)));
    await push("co2", 2600);
    check("ep70 AirQuality: CO2 2600 ppm -> VeryPoor (the worst pollutant wins)",
        (await level()) === AQ.VeryPoor && (await reported(() => level(false), AQ.VeryPoor)));
    await push("tvoc", 50000);
    check("ep70 TVOC: reported (50000 ppb) but NOT graded — the level does not move",
        (await tvoc.getMeasuredValueAttribute(true)) === 50000 && (await level()) === AQ.VeryPoor);
    await push("pm25", 5000);
    check("ep70 PM2.5: 5000 ug/m3 is outside any real sensor's range -> null, and it stops counting",
        (await pm25.getMeasuredValueAttribute(true)) === null && (await level()) === AQ.VeryPoor);
    await push("co2", 6000);
    check("ep70 AirQuality: CO2 6000 ppm -> ExtremelyPoor", (await level()) === AQ.ExtremelyPoor);
    await push("aq_temp", 22.5);
    await push("aq_rh", 51);
    {
        const t = await ep.getClusterClient(TemperatureMeasurement).getMeasuredValueAttribute(true);
        const h = await ep.getClusterClient(RelativeHumidityMeasurement).getMeasuredValueAttribute(true);
        check("ep70 temperature and humidity ride on the same endpoint", t === 2250 && h === 5100, `t=${t} rh=${h}`);
    }
}
if (byNumber.has(71)) {
    const ep = byNumber.get(71);
    const aq = ep.getClusterClient(AirQuality);
    const AQ = AirQuality.AirQualityEnum;
    const level = () => aq.getAirQualityAttribute(true);
    check("ep71 smart monitor: no level yet -> Unknown", (await level()) === AQ.Unknown);
    await push("pm10", 400);   // would be VeryPoor if derived
    check("ep71 smart monitor: with a device-computed level configured, PM10 alone does not invent one",
        (await level()) === AQ.Unknown);
    await push("aq_level", 2);
    check("ep71 smart monitor: the device's own level (Fair) wins over the one derived from PM10 400",
        (await level()) === AQ.Fair && (await reported(() => aq.getAirQualityAttribute(false), AQ.Fair)));
    await push("aq_level", 9);
    check("ep71 smart monitor: an invalid device level is Unknown, not silently replaced", (await level()) === AQ.Unknown);
    check("ep71 smart monitor: mandatory Identify cluster present",
        (await ep.getClusterClient(Identify).getIdentifyTypeAttribute(true).catch(() => undefined)) !== undefined);
}

// ---- Camera-AI sources and the occupancy hold time --------------------------
if (byNumber.has(62)) {
    check("ep61: a synthetic-camera `ai:` source without allow_mock is refused (endpoint absent)", !byNumber.has(61));
    const ai = byNumber.get(60).getClusterClient(OccupancySensing);
    const aiOcc = await ai.getOccupancyAttribute(true).catch(() => undefined);
    check("ep60 AI occupancy: the AI engine never started -> unavailable, NOT 'nobody there'",
        aiOcc === undefined || aiOcc === null, show(aiOcc));
    check("ep60 AI occupancy: advertises the VISION technology (Matter 1.5)",
        (await ai.getFeatureMapAttribute(true))?.vision === true);

    const held = byNumber.get(62).getClusterClient(OccupancySensing);
    const occupied = async (remote) => (await held.getOccupancyAttribute(remote))?.occupied;
    await push("held_presence", true);
    check("ep62 held presence: occupied as soon as the source is true (the hold never delays the rising edge)",
        (await occupied(true)) === true);
    await push("held_presence", false);
    await sleep(600);
    check("ep62 held presence: still occupied 0.6 s after the source cleared (hold 1.5 s)",
        (await occupied(true)) === true);
    check("ep62 held presence: unoccupied once the hold has passed, REPORTED to subscribed controller",
        await reported(() => occupied(false), false, 5000));
}

// ---- Config-driven actuators ([[matter.endpoints]], endpoints.toml) ---------
// Commands go in over Matter; the `signal:` sinks publish what the device was
// really told to do, which GET /signals exposes — so each check proves the
// command reached the output side, not just that an attribute changed.
async function signalsNow() {
    const r = await fetch(`http://127.0.0.1:${metricsPort}/signals`, {
        headers: { Authorization: `Bearer ${token}` },
    });
    return r.json();
}
const rejects = async (fn) => { try { await fn(); return false; } catch { return true; } };

if (byNumber.has(30)) {
    const deviceTypes = async (n) =>
        (await byNumber.get(n).getClusterClient(Descriptor).getDeviceTypeListAttribute(true)).map((d) => Number(d.deviceType));
    check("ep30 light: device type On/Off Light (0x100)", (await deviceTypes(30)).includes(0x100));
    check("ep31 plug: device type On/Off Plug-in Unit (0x10a)", (await deviceTypes(31)).includes(0x10a));
    check("ep32 fan: device type Fan (0x2b)", (await deviceTypes(32)).includes(0x2b));

    // Every actuator starts OFF and says so through its sink.
    {
        const sig = await signalsNow();
        check("actuators boot OFF and told their sinks (lamp/plug/porch false, fan 0)",
            sig["push:lamp"] === false && sig["push:plug"] === false && sig["push:porch"] === false && sig["push:fan"] === 0,
            show(sig));
    }

    // On/Off light, plug, and a second light — each its own endpoint and sink.
    const drive = async (n, sigName, label) => {
        const c = byNumber.get(n).getClusterClient(OnOff);
        check(`ep${n} ${label}: boots OFF`, (await c.getOnOffAttribute(true)) === false);
        await c.on();
        check(`ep${n} ${label}: on() -> OnOff true`, (await c.getOnOffAttribute(true)) === true);
        check(`ep${n} ${label}: on() reached the sink (push:${sigName} = true)`, (await signalsNow())[`push:${sigName}`] === true);
        check(`ep${n} ${label}: change REPORTED to subscribed controller`,
            await reported(() => c.getOnOffAttribute(false), true));
        await c.off();
        check(`ep${n} ${label}: off() -> OnOff false and sink false`,
            (await c.getOnOffAttribute(true)) === false && (await signalsNow())[`push:${sigName}`] === false);
        await c.toggle();
        check(`ep${n} ${label}: toggle() -> true and sink true`,
            (await c.getOnOffAttribute(true)) === true && (await signalsNow())[`push:${sigName}`] === true);
        await c.off();
    };
    await drive(30, "lamp", "light");
    await drive(31, "plug", "plug");
    {
        // The two lights are independent: driving one must not move the other.
        const lamp = byNumber.get(30).getClusterClient(OnOff);
        const porch = byNumber.get(33).getClusterClient(OnOff);
        await porch.on();
        const sig = await signalsNow();
        check("ep33 second light is independent of ep30 (porch on, lamp still off)",
            (await porch.getOnOffAttribute(true)) === true && (await lamp.getOnOffAttribute(true)) === false
            && sig["push:porch"] === true && sig["push:lamp"] === false, show(sig));
        await porch.off();
    }

    // The light carries LIGHTING and its attributes; the plug is plain OnOff.
    {
        const light = byNumber.get(30).getClusterClient(OnOff);
        const plug = byNumber.get(31).getClusterClient(OnOff);
        const lightFeat = await light.getFeatureMapAttribute(true);
        const plugFeat = await plug.getFeatureMapAttribute(true);
        check("ep30 light: advertises the LIGHTING feature", lightFeat?.lighting === true, show(lightFeat));
        check("ep31 plug: does not claim LIGHTING", plugFeat?.lighting !== true, show(plugFeat));
        const lightAttrs = (await light.getAttributeListAttribute(true)).map(Number);
        const plugAttrs = (await plug.getAttributeListAttribute(true)).map(Number);
        const lightingOnly = [0x4000, 0x4001, 0x4002, 0x4003];
        check("ep30 light: advertises GlobalSceneControl/OnTime/OffWaitTime/StartUpOnOff",
            lightingOnly.every((a) => lightAttrs.includes(a)), show(lightAttrs));
        check("ep31 plug: advertises none of the LIGHTING-only attributes",
            lightingOnly.every((a) => !plugAttrs.includes(a)), show(plugAttrs));
    }

    // Fan, three real speeds (ep32).
    {
        const fan = byNumber.get(32).getClusterClient(FanControl);
        const read = async () => ({
            mode: await fan.getFanModeAttribute(true),
            setting: await fan.getPercentSettingAttribute(true),
            current: await fan.getPercentCurrentAttribute(true),
        });
        const FM = FanControl.FanMode;
        check("ep32 fan: FanModeSequence = OffLowMedHigh",
            (await fan.getFanModeSequenceAttribute(true)) === FanControl.FanModeSequence.OffLowMedHigh);
        let st = await read();
        check("ep32 fan: boots Off / 0% / 0%", st.mode === FM.Off && st.setting === 0 && st.current === 0, show(st));

        await fan.setFanModeAttribute(FM.Medium);
        st = await read();
        check("ep32 fan: FanMode=Medium -> PercentSetting 66, PercentCurrent 66",
            st.mode === FM.Medium && st.setting === 66 && st.current === 66, show(st));
        check("ep32 fan: FanMode=Medium reached the sink (push:fan = 66)", (await signalsNow())["push:fan"] === 66);
        // A FanMode write moves PercentSetting and PercentCurrent too: all three
        // must be reported, not just the attribute that was written.
        check("ep32 fan: FanMode, PercentSetting AND PercentCurrent changes all REPORTED to subscribed controller",
            (await reported(() => fan.getFanModeAttribute(false), FM.Medium))
            && (await reported(() => fan.getPercentSettingAttribute(false), 66))
            && (await reported(() => fan.getPercentCurrentAttribute(false), 66)));

        await fan.setPercentSettingAttribute(40);
        st = await read();
        check("ep32 fan: PercentSetting=40 -> Medium band (setting keeps 40, running at 66)",
            st.mode === FM.Medium && st.setting === 40 && st.current === 66, show(st));
        check("ep32 fan: PercentSetting=40 cascade REPORTED (setting 40)",
            await reported(() => fan.getPercentSettingAttribute(false), 40));

        await fan.setPercentSettingAttribute(90);
        st = await read();
        check("ep32 fan: PercentSetting=90 -> High (running at 100)",
            st.mode === FM.High && st.setting === 90 && st.current === 100, show(st));
        check("ep32 fan: High reached the sink (push:fan = 100)", (await signalsNow())["push:fan"] === 100);
        check("ep32 fan: PercentSetting write cascades FanMode=High to subscribers",
            await reported(() => fan.getFanModeAttribute(false), FM.High));

        await fan.setPercentSettingAttribute(20);
        st = await read();
        check("ep32 fan: PercentSetting=20 -> Low band (running at 33)",
            st.mode === FM.Low && st.setting === 20 && st.current === 33, show(st));

        // Refused writes, and they must leave the state exactly where it was.
        const before = await read();
        check("ep32 fan: FanMode=Auto refused (no Auto feature)", await rejects(() => fan.setFanModeAttribute(FM.Auto)));
        check("ep32 fan: FanMode=On refused (deprecated mode)", await rejects(() => fan.setFanModeAttribute(FM.On)));
        check("ep32 fan: PercentSetting=null refused (no automatic mode)", await rejects(() => fan.setPercentSettingAttribute(null)));
        check("ep32 fan: PercentSetting=101 refused (constraint)", await rejects(() => fan.setPercentSettingAttribute(101)));
        check("ep32 fan: refused writes left the state untouched", show(await read()) === show(before), show(await read()));

        await fan.setFanModeAttribute(FM.Off);
        st = await read();
        check("ep32 fan: FanMode=Off -> 0% / 0% and sink 0",
            st.mode === FM.Off && st.setting === 0 && st.current === 0 && (await signalsNow())["push:fan"] === 0, show(st));

        const accepted = (await fan.getAcceptedCommandListAttribute(true)).map(Number);
        check("ep32 fan: no commands advertised (the Step feature is not claimed)", accepted.length === 0, show(accepted));
    }

    // Fan, one real speed (ep34, the plain-relay shape): only Off / High.
    {
        const fan = byNumber.get(34).getClusterClient(FanControl);
        const FM = FanControl.FanMode;
        check("ep34 fan: FanModeSequence = OffHigh",
            (await fan.getFanModeSequenceAttribute(true)) === FanControl.FanModeSequence.OffHigh);
        check("ep34 fan: Low refused (this fan has one speed)", await rejects(() => fan.setFanModeAttribute(FM.Low)));
        check("ep34 fan: Medium refused (this fan has one speed)", await rejects(() => fan.setFanModeAttribute(FM.Medium)));
        await fan.setFanModeAttribute(FM.High);
        check("ep34 fan: High -> 100% / 100%",
            (await fan.getPercentSettingAttribute(true)) === 100 && (await fan.getPercentCurrentAttribute(true)) === 100);
        await fan.setPercentSettingAttribute(1);
        check("ep34 fan: any non-zero percent runs the one speed",
            (await fan.getFanModeAttribute(true)) === FM.High && (await fan.getPercentCurrentAttribute(true)) === 100);
        await fan.setFanModeAttribute(FM.Off);
        check("ep34 fan: Off -> 0%", (await fan.getPercentCurrentAttribute(true)) === 0);
    }

    // Mandatory Identify cluster on every actuator.
    for (const n of [30, 31, 32, 33, 34]) {
        const c = byNumber.get(n).getClusterClient(Identify);
        let ok = false;
        try { ok = (await c.getIdentifyTypeAttribute(true)) !== undefined; } catch { /* missing */ }
        check(`ep${n}: mandatory Identify cluster present`, ok);
    }

    // Endpoints sharing a device type must carry distinct TagLists.
    for (const [a, b, what] of [[30, 33, "lights (0x100)"], [32, 34, "fans (0x2b)"]]) {
        const ta = await byNumber.get(a).getClusterClient(Descriptor).getTagListAttribute(true);
        const tb = await byNumber.get(b).getClusterClient(Descriptor).getTagListAttribute(true);
        check(`ep${a}/ep${b} share a device type (${what}): non-empty, distinct TagLists`,
            (ta?.length ?? 0) > 0 && (tb?.length ?? 0) > 0 && show(ta) !== show(tb), `${show(ta)} | ${show(tb)}`);
    }
}


// ---- Generic Switch events ([[matter.endpoints]], endpoints.toml) -----------
// A button press is something that HAPPENED, so controllers learn of it through
// Switch-cluster events, not an attribute. Press it over HTTP (push:button) and
// collect the events the way a real controller does: as they are DELIVERED over
// its subscription (reading the log back is not the same thing — matter.js's own
// getters skip events the subscription already handed over).
if (byNumber.has(40)) {
    const button = byNumber.get(40).getClusterClient(Switch);
    const rocker = byNumber.get(41).getClusterClient(Switch);
    listenForEvents(40, button, ["InitialPress", "LongPress", "ShortRelease", "LongRelease",
        "MultiPressOngoing", "MultiPressComplete"]);
    listenForEvents(41, rocker, ["SwitchLatched"]);
    const brief = (evs) => evs.map((e) => {
        const d = e.data ?? {};
        switch (e.name) {
            case "InitialPress": case "LongPress": case "SwitchLatched": return `${e.name}(${d.newPosition})`;
            case "ShortRelease": case "LongRelease": return `${e.name}(${d.previousPosition})`;
            case "MultiPressOngoing": return `${e.name}(${d.newPosition},#${d.currentNumberOfPressesCounted})`;
            case "MultiPressComplete": return `${e.name}(${d.previousPosition},total ${d.totalNumberOfPressesCounted})`;
            default: return e.name;
        }
    });
    const same = (got, want) => JSON.stringify(got) === JSON.stringify(want);

    // Structure.
    const types = (await byNumber.get(40).getClusterClient(Descriptor).getDeviceTypeListAttribute(true)).map((d) => Number(d.deviceType));
    check("ep40 button: device type Generic Switch (0xf)", types.includes(0xf), show(types));
    const feats = await button.getFeatureMapAttribute(true);
    check("ep40 button: momentary switch with release, long press and multi press",
        feats?.momentarySwitch && feats?.momentarySwitchRelease && feats?.momentarySwitchLongPress
        && feats?.momentarySwitchMultiPress && !feats?.latchingSwitch, show(feats));
    check("ep40 button: NumberOfPositions = 2, MultiPressMax = 3 (as configured)",
        (await button.getNumberOfPositionsAttribute(true)) === 2 && (await button.getMultiPressMaxAttribute(true)) === 3);
    check("ep40 button: rests at position 0 before anything is pressed",
        (await button.getCurrentPositionAttribute(true)) === 0);
    {
        const idOk = (await byNumber.get(40).getClusterClient(Identify).getIdentifyTypeAttribute(true)) !== undefined;
        check("ep40/41: mandatory Identify cluster present",
            idOk && (await byNumber.get(41).getClusterClient(Identify).getIdentifyTypeAttribute(true)) !== undefined);
    }
    check("ep40 button: no events before anything is pressed", eventsOf(40).length === 0);

    // Short press.
    await push("button", true); await sleep(120); await push("button", false);
    let got = brief(await nextEvents(40, 3));
    check("ep40 short press: InitialPress, ShortRelease, MultiPressComplete(total 1) — no long press, DELIVERED to the subscribed controller",
        same(got, ["InitialPress(1)", "ShortRelease(1)", "MultiPressComplete(1,total 1)"]), show(got));

    // Double press.
    await push("button", true); await sleep(80); await push("button", false); await sleep(80);
    await push("button", true); await sleep(80); await push("button", false);
    got = brief(await nextEvents(40, 6));
    check("ep40 double press: counts two (MultiPressOngoing #2, MultiPressComplete total 2)",
        same(got, ["InitialPress(1)", "ShortRelease(1)", "InitialPress(1)", "MultiPressOngoing(1,#2)",
            "ShortRelease(1)", "MultiPressComplete(1,total 2)"]), show(got));

    // Long press, including CurrentPosition while held.
    await push("button", true);
    check("ep40 long press: CurrentPosition = 1 while held, REPORTED to subscribed controller",
        await reported(() => button.getCurrentPositionAttribute(false), 1));
    await sleep(700);
    await push("button", false);
    got = brief(await nextEvents(40, 3));
    check("ep40 long press: InitialPress, LongPress, LongRelease — no ShortRelease, no MultiPressComplete",
        same(got, ["InitialPress(1)", "LongPress(1)", "LongRelease(1)"]), show(got));
    check("ep40 long press: CurrentPosition back to 0, REPORTED",
        await reported(() => button.getCurrentPositionAttribute(false), 0));

    // Latching rocker.
    check("ep41 rocker: latching switch feature, no momentary ones",
        (await rocker.getFeatureMapAttribute(true))?.latchingSwitch === true
        && (await rocker.getFeatureMapAttribute(true))?.momentarySwitch !== true);
    const noPos = await rocker.getCurrentPositionAttribute(true).catch(() => undefined);
    check("ep41 rocker: no reading -> CurrentPosition unavailable, not a made-up 0", noPos === undefined || noPos === null);
    await push("rocker", false);
    await sleep(300);
    check("ep41 rocker: first reading adopted silently (position 0, no event)",
        (await rocker.getCurrentPositionAttribute(true)) === 0 && eventsOf(41).length === 0);
    await push("rocker", true);
    got = brief(await nextEvents(41, 1));
    check("ep41 rocker: moving it emits SwitchLatched(1)", same(got, ["SwitchLatched(1)"]), show(got));
    check("ep41 rocker: CurrentPosition = 1, REPORTED", await reported(() => rocker.getCurrentPositionAttribute(false), 1));
    await push("rocker", false);
    got = brief(await nextEvents(41, 1));
    check("ep41 rocker: moving it back emits SwitchLatched(0)", same(got, ["SwitchLatched(0)"]), show(got));
    check("ep40/ep41 share a device type: distinct, non-empty TagLists", await (async () => {
        const a = await byNumber.get(40).getClusterClient(Descriptor).getTagListAttribute(true);
        const b = await byNumber.get(41).getClusterClient(Descriptor).getTagListAttribute(true);
        return (a?.length ?? 0) > 0 && (b?.length ?? 0) > 0 && show(a) !== show(b);
    })());
}

const failed = results.filter((r) => !r.ok);
console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
await controller.close();
if (!args.keep) fs.rmSync(storage, { recursive: true, force: true });
process.exit(failed.length ? 1 : 0);
