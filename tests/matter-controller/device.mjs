// Real-device verification (matter.js): commission a PHYSICAL fusion-firmware node
// over IP as an independent Matter controller and exercise what is on it.
//
//   FUSION_TOKEN=<command_token> node device.mjs --ip 192.168.1.17 \
//       [--http http://192.168.1.17:9100] [--port 5540] [--passcode 20202021] \
//       [--discriminator 3840] [--sw-version 1.2.0] [--soc-c 75.6] [--gpu-c 71.1] [--keep]
//       [--storage DIR] [--reconnect]
//
// --storage DIR  keep this controller's fabric in DIR (default: a temp dir, deleted at the end)
// --reconnect    do NOT commission: reuse the fabric in --storage and reconnect to the node it
//                paired with — proves a pairing survives a device restart (run it after
//                restarting the service)
//
// What it does, in order:
//   1. commissions the node (PASE + CASE) — it must be UNCOMMISSIONED;
//   2. dumps every endpoint (device types, clusters, tag lists, unique ids);
//   3. SWEEPS every attribute of every cluster on every endpoint and reports any
//      that cannot be read — the broadest conformance smoke test there is;
//   4. checks the REAL sensors (temperatures against the board's own readings, the
//      camera-AI / motion occupancy sensors are readable);
//   5. exercises only the VIRTUAL devices (plug, fan, button, legacy relay/light/
//      thermostat) — commands over Matter, state observed on the device's
//      `GET /signals` — and leaves them off.
// The token is read from the environment so it never lands in a shell history or log.
//
// The device is left COMMISSIONED into this script's throwaway fabric: reset its
// Matter state afterwards (stop the service, delete config/matter_state/k_*, start)
// before pairing it with a real controller.
import "@matter/nodejs";
import { Environment } from "@matter/main";
import { CommissioningController } from "@project-chip/matter.js";
import {
    OnOff, LevelControl, ColorControl, Thermostat, Descriptor, BasicInformation,
    GeneralCommissioning, TemperatureMeasurement, OccupancySensing, Identify,
    FanControl, Switch,
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
const ip = args.ip;
if (!ip) { console.error("usage: node device.mjs --ip <device-ip> [...]"); process.exit(2); }
const port = Number(args.port ?? 5540);
const passcode = Number(args.passcode ?? 20202021);
const discriminator = Number(args.discriminator ?? 3840);
const http = args.http ?? `http://${ip}:9100`;
const token = process.env.FUSION_TOKEN ?? "";
const reconnect = args.reconnect === true;
const keepStorage = typeof args.storage === "string";
const storage = keepStorage ? args.storage : fs.mkdtempSync(path.join(os.tmpdir(), "fusion-device-"));
if (keepStorage) fs.mkdirSync(storage, { recursive: true });

const results = [];
function check(name, ok, detail = "") {
    results.push({ name, ok });
    console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? "  — " + detail : ""}`);
}
const note = (text) => console.log(`NOTE  ${text}`);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const show = (v) => JSON.stringify(v, (_k, x) => (typeof x === "bigint" ? Number(x) : x));

// ---- the device's HTTP API (signals): how virtual devices are observed/driven ---------
const auth = token ? { Authorization: `Bearer ${token}` } : {};
const signals = async () => (await fetch(`${http}/signals`, { headers: auth })).json();
const push = async (name, value) =>
    (await fetch(`${http}/signals/${name}`, {
        method: "POST",
        headers: { ...auth, "Content-Type": "application/json" },
        body: JSON.stringify({ value }),
    })).status;

// ---- events, the way a real controller sees them: delivered over its subscription ----
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
async function nextEvents(ep, count, timeoutMs = 5000) {
    const end = Date.now() + timeoutMs;
    const fresh = () => eventsOf(ep).slice(eventConsumed[ep] ?? 0);
    while (fresh().length < count && Date.now() < end) await sleep(50);
    if (count > 0) await sleep(400);
    const got = fresh();
    eventConsumed[ep] = (eventConsumed[ep] ?? 0) + got.length;
    return got;
}
const brief = (evs) => evs.map((e) => {
    const d = e.data ?? {};
    switch (e.name) {
        case "InitialPress": case "LongPress": return `${e.name}(${d.newPosition})`;
        case "ShortRelease": case "LongRelease": return `${e.name}(${d.previousPosition})`;
        case "MultiPressOngoing": return `${e.name}(${d.newPosition},#${d.currentNumberOfPressesCounted})`;
        case "MultiPressComplete": return `${e.name}(${d.previousPosition},total ${d.totalNumberOfPressesCounted})`;
        default: return e.name;
    }
});
async function reported(read, expected, ms = 6000) {
    const end = Date.now() + ms;
    for (;;) {
        if ((await read()) === expected) return true;
        if (Date.now() > end) return false;
        await sleep(100);
    }
}

// ---- commission ---------------------------------------------------------------------------
const environment = Environment.default;
environment.vars.set("path.root", storage);
const controller = new CommissioningController({
    environment: { environment, id: "fusion-device-verify" },
    autoConnect: false,
    adminFabricLabel: "fusion-device-verify",
});
await controller.start();
let nodeId;
if (reconnect) {
    const ids = controller.getCommissionedNodes();
    if (ids.length === 0) {
        check("reconnect: this storage holds a previously commissioned node", false, storage);
        await controller.close();
        process.exit(2);
    }
    nodeId = ids[0];
    check("reconnect: this storage holds a previously commissioned node", true, `node id ${nodeId}`);
    try {
        const node = await controller.connectNode(nodeId);
        // The proof of a working CASE session is an actual read over it.
        const vendor = await node.getRootClusterClient(BasicInformation).getVendorNameAttribute(true);
        check("reconnect: reconnected to the commissioned node after its restart (a read over a fresh CASE session)",
            typeof vendor === "string" && vendor.length > 0, vendor);
    } catch (e) {
        check("reconnect: reconnected to the commissioned node after its restart (a read over a fresh CASE session)", false, String(e?.message ?? e));
        await controller.close();
        process.exit(2);
    }
} else {
console.log(`commissioning ${ip}:${port} (passcode ${passcode}, discriminator ${discriminator})`);
try {
    nodeId = await controller.commissionNode({
        commissioning: {
            regulatoryLocation: GeneralCommissioning.RegulatoryLocationType.IndoorOutdoor,
            regulatoryCountryCode: "XX",
        },
        discovery: {
            knownAddress: { ip, port, type: "udp" },
            identifierData: { longDiscriminator: discriminator },
            timeout: 60,
        },
        passcode,
    });
} catch (e) {
    check("commission the physical node over IP (PASE + CASE)", false, String(e?.message ?? e));
    await controller.close();
    process.exit(2);
}
check("commission the physical node over IP (PASE + CASE)", true, `node id ${nodeId}`);
}

const node = await controller.getNode(nodeId);
if (!node.initialized) await node.events.initialized;

// ---- structure -----------------------------------------------------------------------------
const root = node.getRootClusterClient(BasicInformation);
const info = {
    vendor: await root.getVendorNameAttribute(true),
    product: await root.getProductNameAttribute(true),
    sw: await root.getSoftwareVersionStringAttribute(true),
    swNum: await root.getSoftwareVersionAttribute(true),
    serial: await root.getSerialNumberAttribute(true).catch(() => undefined),
};
note(`BasicInformation: ${show(info)}`);
check("BasicInformation: vendor and product are set", !!info.vendor && !!info.product, `${info.vendor} / ${info.product}`);
if (args["sw-version"]) {
    check("BasicInformation: SoftwareVersionString is this build's version", info.sw === args["sw-version"], info.sw);
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
        try { const t = await desc.getTagListAttribute?.(); if (t) extra += ` tags=${show(t.map((x) => x.label))}`; } catch { /* none */ }
        try { const u = await desc.getEndpointUniqueIdAttribute?.(); if (u) extra += ` uniqueId=${u}`; } catch { /* none */ }
    }
    console.log(`  endpoint ${ep.number}: types=[${types}] servers=[${servers}]${extra}`);
}

// ---- 3. sweep: read EVERY attribute each cluster ADVERTISES, on EVERY endpoint ----------------
// Only attributes in the server's own AttributeList count: matter.js's client also knows the
// spec's optional attributes the server does not implement, and those are not findings. An
// advertised attribute that cannot be read IS (a Matter handler that lists something it
// cannot answer).
{
    let ok = 0;
    let advertised = 0;
    const bad = [];
    for (const ep of endpoints) {
        for (const client of ep.getAllClusterClients()) {
            // The five global attributes are mandatory on EVERY cluster.
            for (const g of ["attributeList", "featureMap", "clusterRevision", "acceptedCommandList", "generatedCommandList"]) {
                try {
                    if ((await client.attributes?.[g]?.get(true)) === undefined) bad.push(`ep${ep.number} ${client.name}.${g}: mandatory global attribute unreadable`);
                } catch (e) {
                    bad.push(`ep${ep.number} ${client.name}.${g}: ${String(e?.message ?? e).slice(0, 50)}`);
                }
            }
            const listed = new Set(((await client.attributes?.attributeList?.get(true).catch(() => undefined)) ?? []).map(Number));
            if (listed.size === 0) { bad.push(`ep${ep.number} ${client.name}: AttributeList empty/unreadable`); continue; }
            for (const [name, attr] of Object.entries(client.attributes ?? {})) {
                if (!listed.has(Number(attr.id))) continue; // not advertised: not this server's
                advertised++;
                try {
                    const v = await attr.get(true);
                    if (v === undefined) bad.push(`ep${ep.number} ${client.name}.${name}: advertised but unavailable`);
                    else ok++;
                } catch (e) {
                    bad.push(`ep${ep.number} ${client.name}.${name}: ${String(e?.message ?? e).slice(0, 80)}`);
                }
            }
        }
    }
    console.log(`sweep: ${ok}/${advertised} advertised attributes read cleanly across ${endpoints.length} endpoints`);
    for (const b of bad.slice(0, 40)) console.log(`  unreadable: ${b}`);
    check("sweep: every advertised attribute AND every mandatory global attribute is readable", bad.length === 0 && advertised > 0, bad.length ? `${bad.length} unreadable` : `${ok} attributes`);
}

// mandatory Identify on everything but the camera
for (const [n, ep] of byNumber) {
    if (n === 0 || n === 1) continue;
    const c = ep.getClusterClient(Identify);
    let present = false;
    try { present = c !== undefined && (await c.getIdentifyTypeAttribute(true)) !== undefined; } catch { /* absent */ }
    check(`ep${n}: mandatory Identify cluster present`, present);
}

// ---- 4. REAL sensors -------------------------------------------------------------------------
async function temperature(n) {
    const c = byNumber.get(n)?.getClusterClient(TemperatureMeasurement);
    return c ? c.getMeasuredValueAttribute(true) : undefined;
}
for (const [n, label, expectArg] of [[20, "SoC", "soc-c"], [21, "GPU", "gpu-c"]]) {
    if (!byNumber.has(n)) continue;
    const raw = await temperature(n);
    const celsius = raw === null || raw === undefined ? null : raw / 100;
    check(`ep${n} ${label} temperature: a real, plausible reading`, celsius !== null && celsius > 15 && celsius < 110, `${celsius} C`);
    if (args[expectArg] !== undefined && celsius !== null) {
        const want = Number(args[expectArg]);
        check(`ep${n} ${label} temperature: agrees with the board's own sensor (±6 C)`, Math.abs(celsius - want) <= 6, `matter ${celsius} C vs board ${want} C`);
    }
}
if (byNumber.has(20) && byNumber.has(21)) {
    const a = await byNumber.get(20).getClusterClient(Descriptor).getTagListAttribute(true);
    const b = await byNumber.get(21).getClusterClient(Descriptor).getTagListAttribute(true);
    check("ep20/ep21 share a device type: distinct, non-empty TagLists",
        (a?.length ?? 0) > 0 && (b?.length ?? 0) > 0 && show(a) !== show(b), `${show(a)} | ${show(b)}`);
}
for (const [n, label] of [[22, "camera-AI person"], [23, "camera motion"]]) {
    if (!byNumber.has(n)) continue;
    const c = byNumber.get(n).getClusterClient(OccupancySensing);
    const occ = await c.getOccupancyAttribute(true).catch(() => undefined);
    check(`ep${n} ${label}: the real source is alive (a true/false reading, not 'unavailable')`,
        typeof occ?.occupied === "boolean", show(occ));
    check(`ep${n} ${label}: advertises the VISION technology and the OccupancyChanged event`,
        (await c.getFeatureMapAttribute(true))?.vision === true && (await c.getFeatureMapAttribute(true))?.occupancyEvent === true);
    note(`ep${n} ${label} currently reads occupied=${occ?.occupied}`);
}

// ---- 5. VIRTUAL devices: commands over Matter, observed on the device's /signals -------------
const haveApi = token !== "";
if (!haveApi) note("FUSION_TOKEN not set: skipping the checks that read GET /signals");

if (byNumber.has(30)) {
    const plug = byNumber.get(30).getClusterClient(OnOff);
    check("ep30 plug: boots OFF", (await plug.getOnOffAttribute(true)) === false);
    await plug.on();
    check("ep30 plug: on() -> OnOff true", (await plug.getOnOffAttribute(true)) === true);
    check("ep30 plug: change REPORTED to the subscribed controller", await reported(() => plug.getOnOffAttribute(false), true));
    if (haveApi) check("ep30 plug: the command reached its sink (push:test_plug = true)", (await signals())["push:test_plug"] === true);
    await plug.toggle();
    check("ep30 plug: toggle() -> false", (await plug.getOnOffAttribute(true)) === false);
    if (haveApi) check("ep30 plug: ...and the sink followed (push:test_plug = false)", (await signals())["push:test_plug"] === false);
}
if (byNumber.has(31)) {
    const fan = byNumber.get(31).getClusterClient(FanControl);
    const FM = FanControl.FanMode;
    await fan.setFanModeAttribute(FM.Medium);
    const st = { mode: await fan.getFanModeAttribute(true), set: await fan.getPercentSettingAttribute(true), cur: await fan.getPercentCurrentAttribute(true) };
    check("ep31 fan: FanMode=Medium -> PercentSetting 66 / PercentCurrent 66", st.mode === FM.Medium && st.set === 66 && st.cur === 66, show(st));
    check("ep31 fan: PercentCurrent change REPORTED (the cascade, not just the written attribute)",
        await reported(() => fan.getPercentCurrentAttribute(false), 66));
    if (haveApi) check("ep31 fan: the speed reached its sink (push:test_fan = 66)", (await signals())["push:test_fan"] === 66);
    let refused = false;
    try { await fan.setFanModeAttribute(FM.Auto); } catch { refused = true; }
    check("ep31 fan: Auto is refused (no automatic mode)", refused);
    await fan.setFanModeAttribute(FM.Off);
    check("ep31 fan: Off -> 0%", (await fan.getPercentCurrentAttribute(true)) === 0);
}
if (byNumber.has(40) && haveApi) {
    const button = byNumber.get(40).getClusterClient(Switch);
    listenForEvents(40, button, ["InitialPress", "LongPress", "ShortRelease", "LongRelease", "MultiPressOngoing", "MultiPressComplete"]);
    const same = (got, want) => JSON.stringify(got) === JSON.stringify(want);
    check("ep40 button: momentary switch with release, long press and multi press",
        (await button.getFeatureMapAttribute(true))?.momentarySwitchMultiPress === true);
    await push("test_button", false); await sleep(300);
    eventConsumed[40] = eventsOf(40).length;

    await push("test_button", true); await sleep(150); await push("test_button", false);
    let got = brief(await nextEvents(40, 3));
    check("ep40 button, short press: InitialPress, ShortRelease, MultiPressComplete(total 1)",
        same(got, ["InitialPress(1)", "ShortRelease(1)", "MultiPressComplete(1,total 1)"]), show(got));

    await push("test_button", true); await sleep(100); await push("test_button", false); await sleep(100);
    await push("test_button", true); await sleep(100); await push("test_button", false);
    got = brief(await nextEvents(40, 6));
    check("ep40 button, double press: counted as two",
        same(got, ["InitialPress(1)", "ShortRelease(1)", "InitialPress(1)", "MultiPressOngoing(1,#2)", "ShortRelease(1)", "MultiPressComplete(1,total 2)"]), show(got));

    await push("test_button", true);
    check("ep40 button: CurrentPosition = 1 while held, REPORTED", await reported(() => button.getCurrentPositionAttribute(false), 1));
    await sleep(1200);
    await push("test_button", false);
    got = brief(await nextEvents(40, 3));
    check("ep40 button, long press: InitialPress, LongPress, LongRelease (no ShortRelease)",
        same(got, ["InitialPress(1)", "LongPress(1)", "LongRelease(1)"]), show(got));
}

// legacy virtual endpoints
if (byNumber.has(2)) {
    const c = byNumber.get(2).getClusterClient(OnOff);
    await c.on();
    const a = await c.getOnOffAttribute(true);
    await c.off();
    const b = await c.getOnOffAttribute(true);
    check("ep2 relay (in-memory): on() then off()", a === true && b === false, `${a} -> ${b}`);
}
if (byNumber.has(3)) {
    const ep = byNumber.get(3);
    const onoff = ep.getClusterClient(OnOff);
    const level = ep.getClusterClient(LevelControl);
    const color = ep.getClusterClient(ColorControl);
    await onoff.on();
    await level.moveToLevel({ level: 120, transitionTime: 0, optionsMask: {}, optionsOverride: {} });
    await color.moveToHueAndSaturation({ hue: 100, saturation: 180, transitionTime: 0, optionsMask: {}, optionsOverride: {} });
    const lv = await level.getCurrentLevelAttribute(true);
    const hue = await color.getCurrentHueAttribute(true);
    check("ep3 light (in-memory): on, level 120, hue 100", lv === 120 && hue === 100, `level=${lv} hue=${hue}`);
    await onoff.off();
}
if (byNumber.has(4)) {
    const t = byNumber.get(4).getClusterClient(Thermostat);
    const local = await t.getLocalTemperatureAttribute(true);
    check("ep4 thermostat: LocalTemperature is the SoC's real reading (not null)", typeof local === "number" && local > 1500, `${local}`);
    const before = await t.getOccupiedHeatingSetpointAttribute(true);
    await t.setOccupiedHeatingSetpointAttribute(2100);
    check("ep4 thermostat: write OccupiedHeatingSetpoint=2100", (await t.getOccupiedHeatingSetpointAttribute(true)) === 2100);
    await t.setOccupiedHeatingSetpointAttribute(before);
}

// a last look at the whole signal bus (also shows the virtual devices ended OFF)
if (haveApi) {
    const sig = await signals();
    check("virtual devices ended OFF (plug false, fan 0)", sig["push:test_plug"] === false && sig["push:test_fan"] === 0, show({ plug: sig["push:test_plug"], fan: sig["push:test_fan"] }));
}

const failed = results.filter((r) => !r.ok);
console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
await controller.close();
if (!args.keep && !keepStorage) fs.rmSync(storage, { recursive: true, force: true });
process.exit(failed.length ? 1 : 0);
