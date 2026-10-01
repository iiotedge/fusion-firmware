// Multi-admin harness (matter.js) for fusion-firmware: one node, TWO controllers.
//
// How a node already in one ecosystem is added to another (Apple Home -> Google Home,
// Alexa, Home Assistant, SmartThings ...): the first controller opens an ENHANCED
// commissioning window on the node ("Turn on pairing mode"), the second commissions it
// with the code that window produced, and both then hold a fabric. This does exactly
// that with two independent controllers (two processes, two fabrics) and checks:
//
//   - the second controller commissions the node through the window the first opened
//   - the node then holds two fabrics, and each controller can still read it
//   - the first controller is undisturbed by the second one joining
//
// It leaves the node with no controller (and so addable again).
//
// usage: node multiadmin.mjs --ip 127.0.0.1 --port 5540 [--passcode 20202021] [--discriminator 3840]
import "@matter/nodejs";
import { Environment } from "@matter/main";
import { ManualPairingCodeCodec } from "@matter/main/types";
import { BasicInformation, GeneralCommissioning, OperationalCredentials } from "@matter/main/clusters";
import { CommissioningController } from "@project-chip/matter.js";
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const args = Object.fromEntries(
    process.argv.slice(2).reduce((acc, a, i, all) => {
        if (a.startsWith("--")) acc.push([a.slice(2), all[i + 1] && !all[i + 1].startsWith("--") ? all[i + 1] : true]);
        return acc;
    }, []),
);
const ip = args.ip ?? "127.0.0.1";
const port = Number(args.port ?? 5540);
const role = args.role ?? "first";
const results = [];
function check(name, ok, detail = "") {
    results.push({ name, ok });
    console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? "  — " + detail : ""}`);
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function controllerAt(storage, id) {
    const environment = Environment.default;
    environment.vars.set("path.root", storage);
    const controller = new CommissioningController({ environment: { environment, id }, autoConnect: false, adminFabricLabel: id });
    await controller.start();
    return controller;
}
const commissioning = { regulatoryLocation: GeneralCommissioning.RegulatoryLocationType.IndoorOutdoor, regulatoryCountryCode: "XX" };

// ---- the second controller: its own process, its own fabric ----------------------------------
if (role === "second") {
    const { passcode, shortDiscriminator } = ManualPairingCodeCodec.decode(String(args.code).replaceAll("-", ""));
    const controller = await controllerAt(args.storage, "fusion-second");
    let ok = false;
    try {
        const nodeId = await controller.commissionNode({
            commissioning,
            discovery: { knownAddress: { ip, port, type: "udp" }, identifierData: { shortDiscriminator }, timeout: 30 },
            passcode,
        });
        check("the second controller commissions the node through the window the first one opened", true, `node id ${nodeId}`);
        const node = await controller.getNode(nodeId);
        if (!node.initialized) await node.events.initialized;
        const vendor = await node.getRootClusterClient(BasicInformation).getVendorNameAttribute(true);
        check("the second controller reads the node over its own fabric", typeof vendor === "string" && vendor.length > 0, vendor);
        ok = true;
    } catch (e) {
        check("the second controller commissions the node through the window the first one opened", false, String(e?.message ?? e));
    }
    await controller.close();
    process.exit(ok ? 0 : 1);
}

// ---- the first controller ---------------------------------------------------------------------
const storageA = fs.mkdtempSync(path.join(os.tmpdir(), "fusion-multi-a-"));
const storageB = fs.mkdtempSync(path.join(os.tmpdir(), "fusion-multi-b-"));
const controller = await controllerAt(storageA, "fusion-first");
let nodeId;
try {
    nodeId = await controller.commissionNode({
        commissioning,
        discovery: { knownAddress: { ip, port, type: "udp" }, identifierData: { longDiscriminator: Number(args.discriminator ?? 3840) }, timeout: 30 },
        passcode: Number(args.passcode ?? 20202021),
    });
} catch (e) {
    check("the first controller commissions the node", false, String(e?.message ?? e));
    await controller.close();
    process.exit(1);
}
check("the first controller commissions the node", true, `node id ${nodeId}`);
const node = await controller.getNode(nodeId);
if (!node.initialized) await node.events.initialized;
for (let i = 0; i < 120 && node.state !== 0; i++) await sleep(250); // NodeStates.Connected

let code;
try {
    ({ manualPairingCode: code } = await node.openEnhancedCommissioningWindow(300));
    check("the first controller opens an enhanced commissioning window (\"turn on pairing mode\")", typeof code === "string" && code.length >= 11, code);
} catch (e) {
    check("the first controller opens an enhanced commissioning window (\"turn on pairing mode\")", false, String(e?.message ?? e));
}

if (code) {
    const second = spawnSync(process.execPath, [fileURLToPath(import.meta.url), "--role", "second", "--ip", ip, "--port", String(port), "--storage", storageB, "--code", code], {
        encoding: "utf8",
        timeout: 120000,
    });
    for (const line of (second.stdout ?? "").replace(/\x1b\[[0-9;]*m/g, "").split("\n")) {
        if (/^(PASS|FAIL)/.test(line)) { console.log(line); results.push({ name: line, ok: line.startsWith("PASS") }); }
    }
    await sleep(500);
    const ops = node.getRootClusterClient(OperationalCredentials);
    const fabrics = await ops.getCommissionedFabricsAttribute(true);
    check("the node now holds two fabrics", fabrics === 2, `${fabrics}`);
    const vendor = await node.getRootClusterClient(BasicInformation).getVendorNameAttribute(true);
    check("the first controller is undisturbed by the second one joining", typeof vendor === "string" && vendor.length > 0, vendor);

    // Leave the node with no controller: remove the other fabric, then ourselves. The
    // Fabrics list is fabric-scoped (a controller only sees its own entry), so the other
    // fabric's index is found the plain way: it is one of the node's 5 slots, and
    // RemoveFabric on an unused index is just refused.
    const own = await ops.getCurrentFabricIndexAttribute(true);
    let removed = 0;
    for (let index = 1; index <= 5; index++) {
        if (index === own) continue;
        try {
            const r = await ops.commands.removeFabric({ fabricIndex: index });
            if (r.statusCode === OperationalCredentials.NodeOperationalCertStatus.Ok) removed++;
        } catch { /* an unused slot */ }
    }
    check("the first controller removes the second one's fabric", removed === 1, `${removed} removed`);
}

// An administrator's window left open when the LAST controller leaves: "turn on pairing
// mode" opens a window with a one-off passcode, and if the controller is then removed
// that window is useless with the setup code a person has. The node must replace it
// with its own so it is addable again at once, not after the window times out.
try {
    await node.openEnhancedCommissioningWindow(300);
    await node.decommission();
    await controller.removeNode(nodeId, false);
    const again = await controller.commissionNode({
        commissioning,
        discovery: { knownAddress: { ip, port, type: "udp" }, identifierData: { longDiscriminator: Number(args.discriminator ?? 3840) }, timeout: 30 },
        passcode: Number(args.passcode ?? 20202021),
    });
    check("an administrator's window left open when the last controller leaves is replaced: the setup code works at once", true, `node id ${again}`);
    const last = await controller.getNode(again);
    if (!last.initialized) await last.events.initialized;
    for (let i = 0; i < 120 && last.state !== 0; i++) await sleep(250);
    await last.decommission();
} catch (e) {
    check("an administrator's window left open when the last controller leaves is replaced: the setup code works at once", false, String(e?.message ?? e));
}
await controller.close();
fs.rmSync(storageA, { recursive: true, force: true });
fs.rmSync(storageB, { recursive: true, force: true });

const failed = results.filter((r) => !r.ok);
console.log(`${results.length - failed.length}/${results.length} checks passed (multi-admin)`);
process.exit(failed.length ? 1 : 0);
