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
    BooleanState, Identify,
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
if (root) {
    check("BasicInformation readable", true,
        `vendor=${await root.getVendorNameAttribute(true)} product=${await root.getProductNameAttribute(true)}`);
}
const endpoints = node.getDevices();
const byNumber = new Map(endpoints.map((e) => [e.number, e]));
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


// ---- Config-driven sensors ([[matter.endpoints]], sensors.toml) -------------
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

const failed = results.filter((r) => !r.ok);
console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
await controller.close();
if (!args.keep) fs.rmSync(storage, { recursive: true, force: true });
process.exit(failed.length ? 1 : 0);
