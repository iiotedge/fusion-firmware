// Attestation + commissioning-data harness (matter.js) for fusion-firmware.
//
// The firmware was booted with `[matter].attestation = "files"` (the four test
// files `--matter-test-attestation` wrote) and a CUSTOM setup passcode and
// discriminator. A real controller proves the whole path:
//
//   - the setup code the firmware prints decodes to the configured passcode and discriminator
//   - the default test passcode no longer adds the node
//   - the configured passcode does, which only works if the controller accepted the
//     DAC/PAI/CD the firmware loaded from FILES (matter.js verifies the chain)
//   - BasicInformation reports the configured vendor and product id
//
// usage: node attestation.mjs --ip 127.0.0.1 --port 5540 --qr MT:... --passcode 31415926
//        --discriminator 2020 [--vendor 65521] [--product 32769]
import "@matter/nodejs";
import { Environment } from "@matter/main";
import { QrPairingCodeCodec } from "@matter/main/types";
import { BasicInformation, GeneralCommissioning } from "@matter/main/clusters";
import { CommissioningController } from "@project-chip/matter.js";
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
const wantPasscode = Number(args.passcode);
const wantDiscriminator = Number(args.discriminator);
const wantVendor = Number(args.vendor ?? 0xfff1);
const wantProduct = Number(args.product ?? 0x8001);

const results = [];
function check(name, ok, detail = "") {
    results.push({ name, ok });
    console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? "  — " + detail : ""}`);
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const show = (v) => JSON.stringify(v, (_k, x) => (typeof x === "bigint" ? Number(x) : x));

const code = QrPairingCodeCodec.decode(String(args.qr))[0];
check("the setup code carries the configured passcode and discriminator",
    code.passcode === wantPasscode && code.discriminator === wantDiscriminator, show({ passcode: code.passcode, discriminator: code.discriminator }));
check("the setup code is not the public test code", code.passcode !== 20202021);
check("the setup code carries the configured vendor and product id", code.vendorId === wantVendor && code.productId === wantProduct, `${code.vendorId}/${code.productId}`);

const environment = Environment.default;
const storage = fs.mkdtempSync(path.join(os.tmpdir(), "fusion-attestation-"));
environment.vars.set("path.root", storage);
const controller = new CommissioningController({
    environment: { environment, id: "fusion-attestation" },
    autoConnect: false,
    adminFabricLabel: "fusion-attestation",
});
await controller.start();
const commissioning = { regulatoryLocation: GeneralCommissioning.RegulatoryLocationType.IndoorOutdoor, regulatoryCountryCode: "XX" };
const add = (passcode, discriminator, timeout) =>
    controller.commissionNode({
        commissioning,
        discovery: { knownAddress: { ip, port, type: "udp" }, identifierData: { longDiscriminator: discriminator }, timeout },
        passcode,
    });

// The built-in test code must NOT work any more.
let refused = false;
try {
    await add(20202021, 3840, 8);
} catch {
    refused = true;
}
check("the default test passcode no longer adds the node", refused);

// The configured one does: PASE with it, then attestation over the loaded files.
let nodeId;
try {
    nodeId = await add(code.passcode, code.discriminator, 30);
    check("the configured passcode adds the node, so the controller accepted the DAC/PAI/CD loaded from files", true, `node id ${nodeId}`);
} catch (e) {
    check("the configured passcode adds the node, so the controller accepted the DAC/PAI/CD loaded from files", false, String(e?.message ?? e));
}
if (nodeId !== undefined) {
    const node = await controller.getNode(nodeId);
    if (!node.initialized) await node.events.initialized;
    const root = node.getRootClusterClient(BasicInformation);
    const vendor = Number(await root.getVendorIdAttribute(true));
    const product = Number(await root.getProductIdAttribute(true));
    check("BasicInformation reports the configured vendor and product id", vendor === wantVendor && product === wantProduct, `${vendor}/${product}`);
    for (let i = 0; i < 120 && node.state !== 0; i++) await sleep(250);
    await node.decommission();
}
await controller.close();
fs.rmSync(storage, { recursive: true, force: true });

const failed = results.filter((r) => !r.ok);
console.log(`${results.length - failed.length}/${results.length} checks passed (attestation files + setup code)`);
process.exit(failed.length ? 1 : 0);
