// Pairing lifecycle harness (matter.js) for fusion-firmware.
//
// What a person does with a Matter device: scan or type the setup code, add it,
// later remove it from their controller, and add it again. Two things are proved
// against an independent controller:
//
//   1. The setup code the firmware prints (`--matter-qr`) is the plain standard
//      one, decodes with matter.js to the passcode/discriminator the node really
//      accepts, and is what a controller commissions with. (An earlier build
//      embedded the serial number as optional TLV, making a 73-character code
//      matter.js cannot decode at all.)
//   2. After the LAST controller removes the node (RemoveFabric, which is what
//      Apple Home's "Remove Accessory" sends) it is addable again WITHOUT a
//      restart: rs-matter never reopens the commissioning window by itself.
//
// usage: node lifecycle.mjs --ip 127.0.0.1 --port 5540 --qr MT:... --manual 3497-011-2332
//        [--log firmware.log] [--cycles 2] [--storage DIR]
import "@matter/nodejs";
import { Environment } from "@matter/main";
import { ManualPairingCodeCodec, QrPairingCodeCodec } from "@matter/main/types";
import { CommissioningController } from "@project-chip/matter.js";
import { BasicInformation, GeneralCommissioning } from "@matter/main/clusters";
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
const qr = String(args.qr ?? "");
const manual = String(args.manual ?? "").replaceAll("-", "");
const cycles = Number(args.cycles ?? 2);
const logPath = args.log;
const storage = args.storage ?? fs.mkdtempSync(path.join(os.tmpdir(), "fusion-lifecycle-"));

const results = [];
function check(name, ok, detail = "") {
    results.push({ name, ok, detail });
    console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? "  — " + detail : ""}`);
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const show = (v) => JSON.stringify(v, (_k, x) => (typeof x === "bigint" ? Number(x) : x));
const logText = () => (logPath ? fs.readFileSync(logPath, "utf8").replace(/\x1b\[[0-9;]*m/g, "") : "");

// ---- 1. the setup code ------------------------------------------------------
let code;
try {
    const decoded = QrPairingCodeCodec.decode(qr);
    code = decoded[0];
    check("the setup code decodes with matter.js", decoded.length === 1, show(code));
} catch (e) {
    check("the setup code decodes with matter.js", false, `${qr}: ${e?.message ?? e}`);
    process.exit(1);
}
check("the setup code is the plain 22-character standard one", qr.length === 22 && code.tlvData === undefined, `${qr.length} characters`);
check("the setup code carries the passcode the node accepts", code.passcode === 20202021, String(code.passcode));
check("the setup code carries the discriminator the node advertises", code.discriminator === 3840, String(code.discriminator));
check("the setup code is for the test vendor/product the node reports", code.vendorId === 0xfff1 && code.productId === 0x8001, `${code.vendorId}/${code.productId}`);
const typed = ManualPairingCodeCodec.decode(manual);
check("the manual code carries the same passcode as the QR", typed.passcode === code.passcode, show(typed));
check("the manual code carries the short form of the same discriminator",
    typed.shortDiscriminator === (code.discriminator >> 8), show(typed));
if (logPath) {
    const log = logText();
    check("the boot log shows the pairing code and the same payload", log.includes(`pairing_code=${args.manual}`) && log.includes(`qr_payload=${qr}`));
    check("the boot log has no QR art for journald to stamp and break", !/[▀▄█]|\[38;5;/.test(log));
}

// ---- 2. add, remove the last controller, add again --------------------------
const environment = Environment.default;
environment.vars.set("path.root", storage);
const controller = new CommissioningController({
    environment: { environment, id: "fusion-lifecycle" },
    autoConnect: false,
    adminFabricLabel: "fusion-lifecycle",
});
await controller.start();

async function addFromTheSetupCode() {
    return controller.commissionNode({
        commissioning: {
            regulatoryLocation: GeneralCommissioning.RegulatoryLocationType.IndoorOutdoor,
            regulatoryCountryCode: "XX",
        },
        discovery: {
            knownAddress: { ip, port, type: "udp" },
            identifierData: { longDiscriminator: code.discriminator },
            timeout: 30,
        },
        passcode: code.passcode,
    });
}

for (let cycle = 1; cycle <= cycles; cycle++) {
    let nodeId;
    try {
        nodeId = await addFromTheSetupCode();
    } catch (e) {
        check(`cycle ${cycle}: add the node with the setup code`, false, String(e?.message ?? e));
        break;
    }
    check(`cycle ${cycle}: add the node with the setup code`, true, `node id ${nodeId}`);

    const node = await controller.getNode(nodeId);
    if (!node.initialized) await node.events.initialized;
    const vendor = await node.getRootClusterClient(BasicInformation)?.getVendorNameAttribute(true);
    check(`cycle ${cycle}: the added node answers`, typeof vendor === "string" && vendor.length > 0, String(vendor));

    const reopenedBefore = (logText().match(/pairing is open again/g) ?? []).length;
    try {
        await node.decommission(); // RemoveFabric of the only fabric: the Home app's "Remove Accessory"
        check(`cycle ${cycle}: remove the last controller (RemoveFabric)`, true);
    } catch (e) {
        check(`cycle ${cycle}: remove the last controller (RemoveFabric)`, false, String(e?.message ?? e));
        break;
    }
    await controller.removeNode(nodeId, false);

    if (logPath) {
        let reopened = false;
        for (let i = 0; i < 40 && !reopened; i++) {
            reopened = (logText().match(/pairing is open again/g) ?? []).length > reopenedBefore;
            if (!reopened) await sleep(250);
        }
        check(`cycle ${cycle}: the node says pairing is open again`, reopened);
    }
}

// The final add of the last cycle: after the last removal the node must still be
// addable. Leave it ADDED OUT (decommissioned) so the caller's next controller can
// take it; this checks the reopening one more time, from the last removal.
let again;
try {
    again = await addFromTheSetupCode();
    check("after the last removal the node can be added again, with no restart", true, `node id ${again}`);
    const node = await controller.getNode(again);
    if (!node.initialized) await node.events.initialized;
    await node.decommission();
    await controller.removeNode(again, false);
    check("and removed again, leaving it addable for the next controller", true);
} catch (e) {
    check("after the last removal the node can be added again, with no restart", false, String(e?.message ?? e));
}
await controller.close();

const failed = results.filter((r) => !r.ok);
console.log(`${results.length - failed.length}/${results.length} checks passed (pairing lifecycle)`);
process.exit(failed.length ? 1 : 0);
