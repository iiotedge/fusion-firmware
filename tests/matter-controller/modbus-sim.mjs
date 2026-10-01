// A minimal Modbus TCP server for the verification harness, so the firmware's
// REAL Modbus driver (iiotedge-lib) can poll something and the `[[tags]]` ->
// signal -> Matter path is exercised end to end, with no PLC and no extra npm
// dependency.
//
//   Modbus TCP   --port 5020      FC01/02 (coils/discrete) and FC03/04 (registers)
//   control      --control 5021   POST /set     {"holding":{"0":[215]},"coils":{"1":[1]}}
//                                 POST /pause   drop every connection and refuse new
//                                               ones (a dead link)
//                                 POST /resume
import http from "node:http";
import net from "node:net";

const args = Object.fromEntries(
    process.argv.slice(2).reduce((acc, a, i, all) => {
        if (a.startsWith("--")) acc.push([a.slice(2), all[i + 1]]);
        return acc;
    }, []),
);
const port = Number(args.port ?? 5020);
const control = Number(args.control ?? 5021);

const holding = new Uint16Array(256);
const coils = new Uint8Array(256);
let paused = false;
const sockets = new Set();

function respond(pdu) {
    const fc = pdu[0];
    const addr = pdu.readUInt16BE(1);
    const count = pdu.readUInt16BE(3);
    const exception = (code) => Buffer.from([fc | 0x80, code]);
    if (fc === 3 || fc === 4) {
        if (count < 1 || count > 125 || addr + count > holding.length) return exception(2);
        const out = Buffer.alloc(2 + count * 2);
        out[0] = fc;
        out[1] = count * 2;
        for (let i = 0; i < count; i++) out.writeUInt16BE(holding[addr + i], 2 + i * 2);
        return out;
    }
    if (fc === 1 || fc === 2) {
        if (count < 1 || count > 2000 || addr + count > coils.length) return exception(2);
        const bytes = Math.ceil(count / 8);
        const out = Buffer.alloc(2 + bytes);
        out[0] = fc;
        out[1] = bytes;
        for (let i = 0; i < count; i++) if (coils[addr + i]) out[2 + (i >> 3)] |= 1 << (i & 7);
        return out;
    }
    return exception(1);
}

net.createServer((sock) => {
    if (paused) return sock.destroy();
    sockets.add(sock);
    sock.on("close", () => sockets.delete(sock));
    sock.on("error", () => {});
    let buf = Buffer.alloc(0);
    sock.on("data", (chunk) => {
        buf = Buffer.concat([buf, chunk]);
        while (buf.length >= 7) {
            const len = buf.readUInt16BE(4); // unit id + PDU
            if (buf.length < 6 + len) break;
            const tid = buf.readUInt16BE(0);
            const unit = buf[6];
            const pdu = buf.subarray(7, 6 + len);
            buf = buf.subarray(6 + len);
            if (paused) return sock.destroy();
            const resp = respond(pdu);
            const head = Buffer.alloc(7);
            head.writeUInt16BE(tid, 0);
            head.writeUInt16BE(0, 2);
            head.writeUInt16BE(resp.length + 1, 4);
            head[6] = unit;
            sock.write(Buffer.concat([head, resp]));
        }
    });
}).listen(port, "127.0.0.1", () => console.log(`modbus-sim: Modbus TCP on ${port}, control on ${control}`));

http.createServer(async (req, res) => {
    let body = "";
    for await (const chunk of req) body += chunk;
    const msg = body ? JSON.parse(body) : {};
    if (req.url === "/set") {
        for (const [addr, values] of Object.entries(msg.holding ?? {})) values.forEach((v, i) => (holding[Number(addr) + i] = v & 0xffff));
        for (const [addr, values] of Object.entries(msg.coils ?? {})) values.forEach((v, i) => (coils[Number(addr) + i] = v ? 1 : 0));
    } else if (req.url === "/pause") {
        paused = true;
        for (const s of sockets) s.destroy();
    } else if (req.url === "/resume") {
        paused = false;
    }
    res.end("ok");
}).listen(control, "127.0.0.1");
