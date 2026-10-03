// Drives the VIRTUAL devices of the demo endpoint set (demo-endpoints.toml) on a
// board, so a controller app can be watched reacting to every kind of device.
//
//   FUSION_TOKEN=<command_token> node demo-device.mjs --seed            # give every virtual sensor a first reading
//   FUSION_TOKEN=...             node demo-device.mjs --demo            # walk every device through its states, narrated
//   FUSION_TOKEN=...             node demo-device.mjs --set test_door=false test_temp=26.5
//   FUSION_TOKEN=...             node demo-device.mjs --press double    # test_button: short | double | triple | long
//   FUSION_TOKEN=...             node demo-device.mjs --rocker          # flip test_rocker
//   FUSION_TOKEN=...             node demo-device.mjs --watch 60        # print what the app commands (lamp, plug, fans)
//   FUSION_TOKEN=...             node demo-device.mjs --list
//
// options: --http http://192.168.1.17:9100   --pause 4 (seconds between demo steps)
//
// A pushed value lives in the firmware's memory until it restarts. With the
// `[signals.initial]` table of demo-endpoints.toml in the board's config the virtual
// sensors come back at their resting values after a restart; without it run --seed
// again, or they have no reading (a controller shows them as "No Response", the honest
// answer for a sensor nobody has fed). The token is read from the environment so it
// never lands in a shell history or a log.
const args = process.argv.slice(2);
const flag = (name) => args.includes(`--${name}`);
const value = (name, fallback) => {
    const i = args.indexOf(`--${name}`);
    return i >= 0 && args[i + 1] && !args[i + 1].startsWith("--") ? args[i + 1] : fallback;
};
const base = value("http", "http://192.168.1.17:9100");
const token = process.env.FUSION_TOKEN ?? "";
const pause = Number(value("pause", 4)) * 1000;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const auth = token ? { Authorization: `Bearer ${token}` } : {};

async function signals() {
    const r = await fetch(`${base}/signals`, { headers: auth });
    if (!r.ok) throw new Error(`GET /signals -> HTTP ${r.status}${r.status === 401 ? " (set FUSION_TOKEN to the board's command_token)" : ""}`);
    return r.json();
}
async function push(name, v) {
    const r = await fetch(`${base}/signals/${name}`, {
        method: "POST",
        headers: { ...auth, "Content-Type": "application/json" },
        body: JSON.stringify({ value: v }),
    });
    if (!r.ok) throw new Error(`POST /signals/${name} -> HTTP ${r.status}`);
}
async function set(values) {
    for (const [name, v] of Object.entries(values)) await push(name, v);
}
const say = (text) => console.log(`\n> ${text}`);

// What every virtual sensor reads at rest: a plausible first reading, so nothing is "no data".
const REST = {
    test_temp: 22.5, test_humidity: 45, test_pressure: 1013, test_lux: 300, test_flow: 0,
    test_door: true, test_leak: false, test_rain: false, test_freeze: false, test_soil: 40,
    test_pir: false, test_radar: false, test_ultra: false, test_touch: false, test_other: false,
    test_co2: 600, test_pm25: 8, test_tvoc: 120, test_aq_temp: 22, test_aq_rh: 45,
    test_aq_level: 1, test_pm10: 10,
    test_button: false, test_rocker: false,
};

// A momentary button is a level that goes true while it is pressed; the firmware turns
// that into InitialPress / ShortRelease / LongPress / MultiPress events.
async function press(kind) {
    const tap = async (ms) => { await push("test_button", true); await sleep(ms); await push("test_button", false); };
    if (kind === "short") await tap(150);
    else if (kind === "double") { await tap(120); await sleep(120); await tap(120); }
    else if (kind === "triple") { await tap(120); await sleep(120); await tap(120); await sleep(120); await tap(120); }
    else if (kind === "long") await tap(1400);
    else throw new Error(`--press short|double|triple|long, not ${kind}`);
}

async function rocker() {
    const now = (await signals())["push:test_rocker"] === true;
    await push("test_rocker", !now);
    return !now;
}

// Each step: what to say, what to push, how long to leave it before the next.
const STEPS = [
    ["TEMPERATURE 'Test temperature': 19 -> 23 -> 27 C", [{ test_temp: 19 }, { test_temp: 23 }, { test_temp: 27 }]],
    ["HUMIDITY 'Test humidity': 30 -> 55 -> 80 %", [{ test_humidity: 30 }, { test_humidity: 55 }, { test_humidity: 80 }]],
    ["PRESSURE 'Test pressure': 990 -> 1013 -> 1030 hPa", [{ test_pressure: 990 }, { test_pressure: 1013 }, { test_pressure: 1030 }]],
    ["LIGHT LEVEL 'Test light level': 2 -> 120 -> 800 -> 20000 lux", [{ test_lux: 2 }, { test_lux: 120 }, { test_lux: 800 }, { test_lux: 20000 }]],
    ["WATER FLOW 'Test water flow': 0 -> 3.5 -> 12 -> 0 m3/h", [{ test_flow: 3.5 }, { test_flow: 12 }, { test_flow: 0 }]],
    ["SOIL MOISTURE 'Test soil moisture': 15 -> 45 -> 80 %", [{ test_soil: 15 }, { test_soil: 45 }, { test_soil: 80 }]],
    ["DOOR 'Test door': OPEN, then closed again", [{ test_door: false }, { test_door: true }]],
    ["WATER LEAK 'Test leak': detected, then clear", [{ test_leak: true }, { test_leak: false }]],
    ["RAIN 'Test rain': detected, then clear", [{ test_rain: true }, { test_rain: false }]],
    ["FREEZE 'Test freeze': detected, then clear", [{ test_freeze: true }, { test_freeze: false }]],
    ["OCCUPANCY 'Test PIR': motion, then none", [{ test_pir: true }, { test_pir: false }]],
    ["OCCUPANCY 'Test radar': presence, then none", [{ test_radar: true }, { test_radar: false }]],
    ["OCCUPANCY 'Test ultrasonic': presence, then none", [{ test_ultra: true }, { test_ultra: false }]],
    ["OCCUPANCY 'Test touch': touched, then released", [{ test_touch: true }, { test_touch: false }]],
    ["OCCUPANCY 'Test presence (held)': a 1 s blip stays 'occupied' for 5 s after it clears", [{ test_other: true }, { test_other: false }]],
    ["AIR QUALITY 'Test air quality': PM2.5 5 -> 25 -> 60 -> 150 -> 320 ug/m3 (Good -> Fair -> Moderate -> Poor -> Extremely poor)",
        [{ test_pm25: 5 }, { test_pm25: 25 }, { test_pm25: 60 }, { test_pm25: 150 }, { test_pm25: 320 }, { test_pm25: 8 }]],
    ["AIR QUALITY 'Test air quality': CO2 600 -> 1200 -> 2500 ppm, TVOC 120 -> 900 ppb", [{ test_co2: 1200, test_tvoc: 400 }, { test_co2: 2500, test_tvoc: 900 }, { test_co2: 600, test_tvoc: 120 }]],
    ["AIR QUALITY 'Test smart monitor': its own level 1 -> 3 -> 6 -> 1 (it wins over the concentrations)", [{ test_aq_level: 3 }, { test_aq_level: 6 }, { test_aq_level: 1 }]],
];

async function demo() {
    await set(REST);
    console.log("Every virtual sensor is at rest. Watch the controller app; each step stays for", pause / 1000, "s.");
    for (const [title, states] of STEPS) {
        say(title);
        for (const s of states) {
            await set(s);
            console.log("   ", JSON.stringify(s));
            await sleep(pause);
        }
    }
    say("BUTTON 'Test button': short press");
    await press("short"); await sleep(pause);
    say("BUTTON 'Test button': double press");
    await press("double"); await sleep(pause);
    say("BUTTON 'Test button': triple press");
    await press("triple"); await sleep(pause);
    say("BUTTON 'Test button': long press");
    await press("long"); await sleep(pause);
    say("ROCKER 'Test rocker' (latching): on, then off");
    console.log("    now", (await rocker()) ? "ON" : "OFF"); await sleep(pause);
    console.log("    now", (await rocker()) ? "ON" : "OFF"); await sleep(pause);
    say("Done. Real sensors need no driving: CPU load / memory move by themselves; hold a phone, cup, bottle or cat up to the camera for the AI ones.");
    say("The lamp, plug and fans are yours to switch in the app: run --watch 60 to see the commands arrive.");
}

async function watch(seconds) {
    const names = ["push:test_lamp", "push:test_plug", "push:test_fan"];
    let last = {};
    const end = Date.now() + seconds * 1000;
    console.log(`Watching ${names.join(", ")} for ${seconds} s - operate them in the app.`);
    while (Date.now() < end) {
        const now = await signals();
        for (const n of names) {
            if (JSON.stringify(now[n]) !== JSON.stringify(last[n])) console.log(`  ${new Date().toLocaleTimeString()}  ${n} = ${JSON.stringify(now[n])}`);
            last[n] = now[n];
        }
        await sleep(500);
    }
}

try {
    if (flag("seed")) {
        await set(REST);
        console.log(`seeded ${Object.keys(REST).length} virtual signals`);
    } else if (flag("demo")) {
        await demo();
    } else if (flag("press")) {
        await press(value("press", "short"));
        console.log("pressed:", value("press", "short"));
    } else if (flag("rocker")) {
        console.log("rocker now", (await rocker()) ? "ON" : "OFF");
    } else if (flag("watch")) {
        await watch(Number(value("watch", 60)));
    } else if (flag("set")) {
        const pairs = args.slice(args.indexOf("--set") + 1).filter((a) => !a.startsWith("--") && a.includes("="));
        for (const p of pairs) {
            const [name, raw] = p.split("=");
            const v = raw === "true" ? true : raw === "false" ? false : Number(raw);
            if (typeof v === "number" && Number.isNaN(v)) throw new Error(`${p}: a number, true or false`);
            await push(name, v);
            console.log("set", name, "=", v);
        }
    } else if (flag("list")) {
        const all = await signals();
        for (const [k, v] of Object.entries(all).sort()) console.log(`  ${k} = ${JSON.stringify(v)}`);
    } else {
        console.log("usage: FUSION_TOKEN=<command_token> node demo-device.mjs --seed | --demo | --set name=value ... | --press short|double|triple|long | --rocker | --watch [S] | --list");
        process.exit(2);
    }
} catch (e) {
    console.error(String(e?.message ?? e));
    process.exit(1);
}
