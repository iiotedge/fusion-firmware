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
    BooleanState, Identify, FanControl,
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

const failed = results.filter((r) => !r.ok);
console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
await controller.close();
if (!args.keep) fs.rmSync(storage, { recursive: true, force: true });
process.exit(failed.length ? 1 : 0);
