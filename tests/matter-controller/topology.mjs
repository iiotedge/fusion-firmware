// ConfigurationVersion harness (matter.js) for fusion-firmware.
//
// A node whose endpoints are decided by a config file changes shape when that file
// does. The spec says it MUST then bump BasicInformation.ConfigurationVersion, which
// is how a controller that already paired it learns to read its structure again
// (without it, a controller may keep showing the old accessory until it is removed
// and added again). run.sh restarts the firmware around this script:
//
//   --mode record   remember the node's ConfigurationVersion and endpoint list in --state
//   --mode same     restart with the SAME config: version and endpoints are unchanged
//   --mode bumped   restart after an endpoint was added: version is exactly one higher
//                   and the new endpoint (--has) is listed
//
// usage: node topology.mjs --storage DIR --state FILE --mode record|same|bumped [--has 99]
// The controller storage is the one controller.mjs kept (--keep): it reconnects to
// the node that was already commissioned, like a real controller after a reboot.
import "@matter/nodejs";
import { Environment } from "@matter/main";
import { BasicInformation, Descriptor } from "@matter/main/clusters";
import { CommissioningController } from "@project-chip/matter.js";
import fs from "node:fs";

const args = Object.fromEntries(
    process.argv.slice(2).reduce((acc, a, i, all) => {
        if (a.startsWith("--")) acc.push([a.slice(2), all[i + 1] && !all[i + 1].startsWith("--") ? all[i + 1] : true]);
        return acc;
    }, []),
);
const mode = args.mode;
const stateFile = args.state;
const has = Number(args.has ?? 99);
if (!args.storage || !stateFile || !["record", "same", "bumped"].includes(mode)) {
    console.error("usage: node topology.mjs --storage DIR --state FILE --mode record|same|bumped [--has N]");
    process.exit(2);
}

const results = [];
function check(name, ok, detail = "") {
    results.push({ name, ok });
    console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? "  — " + detail : ""}`);
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const environment = Environment.default;
environment.vars.set("path.root", args.storage);
const controller = new CommissioningController({
    environment: { environment, id: "fusion-verify" },
    autoConnect: false,
    adminFabricLabel: "fusion-verify",
});
await controller.start();

const ids = controller.getCommissionedNodes();
if (ids.length === 0) {
    check("the controller still holds the commissioned node", false, args.storage);
    await controller.close();
    process.exit(2);
}

let version;
let parts;
try {
    const node = await controller.connectNode(ids[0]);
    // A real read over a fresh CASE session, retried: the firmware has only just restarted.
    for (let i = 0; i < 60 && version === undefined; i++) {
        try {
            version = await node.getRootClusterClient(BasicInformation).getConfigurationVersionAttribute(true);
            parts = (await node.getRootClusterClient(Descriptor).getPartsListAttribute(true)).map(Number).sort((a, b) => a - b);
        } catch {
            version = undefined;
            await sleep(500);
        }
    }
} catch (e) {
    check("reconnect to the restarted node", false, String(e?.message ?? e));
}
if (version === undefined) {
    check("read ConfigurationVersion and the endpoint list after the restart", false);
    await controller.close();
    process.exit(1);
}
check("read ConfigurationVersion and the endpoint list after the restart", true, `version ${version}, ${parts.length} endpoints`);

if (mode === "record") {
    fs.writeFileSync(stateFile, JSON.stringify({ version, parts }));
} else {
    const before = JSON.parse(fs.readFileSync(stateFile, "utf8"));
    if (mode === "same") {
        check("restart with the same config: ConfigurationVersion is unchanged", version === before.version, `${before.version} -> ${version}`);
        check("restart with the same config: the endpoint list is unchanged", JSON.stringify(parts) === JSON.stringify(before.parts));
    } else {
        check(`endpoint ${has} is new`, !before.parts.includes(has) && parts.includes(has), `endpoints ${parts.join(",")}`);
        check("an endpoint was added: ConfigurationVersion is bumped by exactly one", version === before.version + 1, `${before.version} -> ${version}`);
    }
    fs.writeFileSync(stateFile, JSON.stringify({ version, parts }));
}

await controller.close();
const failed = results.filter((r) => !r.ok);
process.exit(failed.length ? 1 : 0);
