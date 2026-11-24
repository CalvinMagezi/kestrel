// Drives the app page in headless Chrome and saves a screenshot of every screen in both themes.
// Usage: node scripts/ui-check.mjs <base-url> <token> <out-dir> [--ask]
// --ask also sends a free-text question, which needs a model key and costs one small request.
import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const [base, token, out] = process.argv.slice(2);
const ask = process.argv.includes("--ask");
if (!base || !token || !out) {
  console.error("usage: node scripts/ui-check.mjs <base-url> <token> <out-dir> [--ask]");
  process.exit(2);
}
fs.mkdirSync(out, { recursive: true });
const CHROME = process.env.CHROME || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const PORT = 9333;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const results = [];
const check = (name, ok, detail = "") => {
  results.push({ name, ok: !!ok, detail });
  console.log(`${ok ? "PASS" : "FAIL"} ${name}${detail ? ` (${detail})` : ""}`);
};

const profile = fs.mkdtempSync(path.join(os.tmpdir(), "fx-ui-"));
const chrome = spawn(CHROME, ["--headless=new", `--remote-debugging-port=${PORT}`, `--user-data-dir=${profile}`, "--no-first-run", "--disable-gpu", "about:blank"], { stdio: "ignore" });

async function connect() {
  for (let i = 0; i < 50; i++) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
      const page = list.find((t) => t.type === "page");
      if (page) return page.webSocketDebuggerUrl;
    } catch { /* not up yet */ }
    await sleep(200);
  }
  throw new Error("Chrome did not start");
}

const ws = new WebSocket(await connect());
await new Promise((r) => ws.addEventListener("open", r, { once: true }));
let id = 0;
let mockUpdate = null;
const pending = new Map();
const problems = [];
ws.addEventListener("message", (m) => {
  const msg = JSON.parse(m.data);
  if (msg.id && pending.has(msg.id)) { pending.get(msg.id)(msg); pending.delete(msg.id); return; }
  if (msg.method === "Fetch.requestPaused") {
    const { requestId, request } = msg.params;
    if (mockUpdate && request.url.includes("/api/update") && request.method === "GET") {
      send("Fetch.fulfillRequest", { requestId, responseCode: 200, responseHeaders: [{ name: "content-type", value: "application/json" }], body: Buffer.from(JSON.stringify(mockUpdate)).toString("base64") });
    } else send("Fetch.continueRequest", { requestId });
    return;
  }
  if (msg.method === "Runtime.exceptionThrown") problems.push(msg.params.exceptionDetails.exception?.description || msg.params.exceptionDetails.text);
  if (msg.method === "Runtime.consoleAPICalled" && msg.params.type === "error") problems.push(msg.params.args.map((a) => a.value ?? a.description).join(" "));
});
const send = (method, params = {}) => new Promise((res, rej) => {
  const i = ++id;
  pending.set(i, (msg) => (msg.error ? rej(new Error(msg.error.message)) : res(msg.result)));
  ws.send(JSON.stringify({ id: i, method, params }));
});
const evalJs = async (expression) => (await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true })).result.value;
const shot = async (name) => {
  const { data } = await send("Page.captureScreenshot", { format: "png" });
  fs.writeFileSync(path.join(out, `${name}.png`), Buffer.from(data, "base64"));
};
const go = async (hash, wait = 1200) => { await evalJs(`location.hash = ${JSON.stringify(hash)}`); await sleep(wait); };
const count = (sel) => evalJs(`document.querySelectorAll(${JSON.stringify(sel)}).length`);
const text = (sel) => evalJs(`(document.querySelector(${JSON.stringify(sel)})||{}).textContent||""`);

await send("Page.enable");
await send("Fetch.enable", { patterns: [{ urlPattern: "*api/update*" }] });
await send("Runtime.enable");

for (const theme of ["dark", "light"]) {
  await send("Emulation.setDeviceMetricsOverride", { width: 390, height: 844, deviceScaleFactor: 2, mobile: true });
  await send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: theme }] });
  await send("Page.navigate", { url: `${base}/?t=${token}#/chat` });
  await sleep(1500);
  await evalJs("localStorage.removeItem('fx-chat'); location.reload()");
  await sleep(1500);
  check(`${theme}: theme applied`, (await evalJs("document.documentElement.dataset.theme")) === theme);
  check(`${theme}: chat has the welcome bubble`, (await count(".k-bubble")) >= 1);
  check(`${theme}: six quick commands`, (await count(".chips .k-chip")) === 6);
  await shot(`${theme}-1-chat`);

  await evalJs("document.getElementById('msg').value='backtest EURUSD'; document.getElementById('form').requestSubmit()");
  await sleep(6000);
  check(`${theme}: backtest card rendered`, (await count(".res")) === 1 && (await count(".res .l")) >= 5);
  check(`${theme}: expectancy has a trend arrow`, (await count(".res .v svg")) >= 1);
  await shot(`${theme}-2-chat-backtest`);

  if (ask && theme === "dark") {
    await evalJs("document.getElementById('msg').value='In two sentences, what does that result mean?'; document.getElementById('form').requestSubmit()");
    await sleep(9000);
    const last = await evalJs("[...document.querySelectorAll('.k-bubble')].pop().textContent");
    check("dark: model answered a free-text question", last.length > 40 && !last.startsWith("Could not"), last.slice(0, 80));
    check("dark: model reply shows no raw markdown", !last.includes("**"));
    check("dark: model reply stays short", last.split(/[.!?]\s/).length <= 8, `${last.length} chars`);
    await shot("dark-3-chat-model");
  }

  await evalJs(`localStorage.setItem('fx-chat', JSON.stringify([{who:'bot',text:'**Bold start**\\n- first point\\n- second <b>point</b>'}])); location.reload()`);
  await sleep(1500);
  check(`${theme}: light formatting renders bold and bullets without parsing HTML`, (await count(".k-bubble strong")) === 1 && (await count(".k-bubble .li")) === 2 && (await count(".k-bubble b")) === 0);
  await go("#/overview", 2500);
  check(`${theme}: overview shows two tiles`, (await count(".k-tile")) === 2);
  check(`${theme}: overview lists the pairs`, (await count("#overview .card .k-row")) >= 3);
  check(`${theme}: header state badge shown`, (await text("#state")).length > 0, await text("#state"));
  check(`${theme}: equity figure fits its tile`, await evalJs("[...document.querySelectorAll('.k-tile__value')].every(e => e.scrollWidth <= e.clientWidth + 1)"));
  await shot(`${theme}-4-overview`);

  await go("#/control", 1500);
  check(`${theme}: control shows pause`, (await text("#control")).includes("Pause entries"));
  check(`${theme}: control shows the model`, (await text("#control")).includes("Haiku") || (await text("#control")).includes("None"));
  check(`${theme}: badges are not clipped`, await evalJs("[...document.querySelectorAll('.k-badge')].every(e => e.scrollHeight <= e.clientHeight + 1)"));
  await shot(`${theme}-5-control`);
  await evalJs("[...document.querySelectorAll('#control button')].find(b=>b.textContent.includes('Pause entries')).click()");
  await sleep(1200);
  check(`${theme}: pausing flips the button and header`, (await text("#control")).includes("Resume entries") && (await text("#state")).includes("Paused"), await text("#state"));
  await shot(`${theme}-6-control-paused`);
  await evalJs("[...document.querySelectorAll('#control button')].find(b=>b.textContent.includes('Resume entries')).click()");
  await sleep(1200);
  check(`${theme}: resuming restores entries`, (await text("#control")).includes("Pause entries"));
  await evalJs("document.querySelector('.stepper button[aria-label=Increase]').click()");
  await sleep(1500);
  check(`${theme}: risk stepper moves`, (await text(".stepper output")) === "0.6%", await text(".stepper output"));
  await evalJs("document.querySelector('.stepper button[aria-label=Decrease]').click()");
  await sleep(1200);

  check(`${theme}: control shows the record and data sections`, (await text("#control")).includes("Your record") && (await text("#control")).includes("Data used") && (await text("#control")).includes("Paper account"));
  check(`${theme}: control shows the journal as intact`, (await text("#control")).includes("Intact"));

  await go("#/learn", 1500);
  check(`${theme}: learn has the five sections`, (await count("#learn .sec")) === 5, String(await count("#learn .sec")));
  check(`${theme}: calculator says a tiny account cannot size this trade`, (await text("#learn .out")).includes("Too small") && (await text("#learn .out")).includes("2.0%"), (await text("#learn .out")).slice(0, 90));
  await evalJs("(() => { const i = document.querySelector('#learn .calc input[aria-label=\"Balance (USD)\"]'); i.value = '1000'; i.dispatchEvent(new Event('input')); })()");
  await sleep(300);
  check(`${theme}: calculator boxes stay inside the screen`, await evalJs("[...document.querySelectorAll('#learn .calc input, #learn .calc select')].every(e => e.getBoundingClientRect().right <= window.innerWidth && e.getBoundingClientRect().left >= 0)"));
  check(`${theme}: calculator sizes a larger account`, (await text("#learn .out")).includes("5,000 units (0.05 lot)"), (await text("#learn .out")).slice(0, 90));
  await shot(`${theme}-6b-learn`);

  await go("#/control", 1200);
  await evalJs("[...document.querySelectorAll('#control button')].find(b=>b.textContent==='Reset').click()");
  await sleep(300);
  check(`${theme}: reset stays disabled until RESET is typed`, await evalJs("document.querySelector('#control .form .k-btn--primary').hasAttribute('disabled')"));
  await evalJs("(() => { const w = document.querySelector('#control .form input[type=text]'); w.value = 'RESET'; w.dispatchEvent(new Event('input')); document.querySelector('#control .form input[type=number]').value = '500'; })()");
  await sleep(200);
  check(`${theme}: reset enables after the word`, !(await evalJs("document.querySelector('#control .form .k-btn--primary').hasAttribute('disabled')")));
  await evalJs("document.querySelector('#control .form .k-btn--primary').click()");
  await sleep(1500);
  await go("#/overview", 1500);
  check(`${theme}: reset set the balance to 500`, (await text("#overview .k-tile__value")).includes("500.00"), await text("#overview .k-tile__value"));
  await go("#/control", 1000);
  await evalJs("[...document.querySelectorAll('#control button')].find(b=>b.textContent==='Reset').click()");
  await sleep(300);
  await evalJs("(() => { const w = document.querySelector('#control .form input[type=text]'); w.value = 'RESET'; w.dispatchEvent(new Event('input')); document.querySelector('#control .form input[type=number]').value = '10000'; })()");
  await evalJs("document.querySelector('#control .form .k-btn--primary').click()");
  await sleep(1500);

  if (theme === "dark") {
    const rel = { version: "0.2.0", notes: "Faster first start.", apk_url: "https://github.com/x", sums_url: "https://github.com/x", size: 12582912, page_url: "https://github.com/CalvinMagezi/kestrel/releases/latest" };
    const base = { current: "0.1.0", progress: 0, error: null, checked_at: Math.floor(Date.now() / 1000), enabled: true, wifi_only: true, dismissed: false, release: rel };
    const state = async (patch) => { mockUpdate = { ...base, ...patch }; await evalJs("location.hash = '#/chat'; location.reload()"); await sleep(1500); };
    await state({ phase: "available" });
    const bar = () => text("#updatebar");
    check("update bar: offers the new version with its size", (await bar()).includes("Kestrel 0.2.0 is available (12 MB)") && (await bar()).includes("Update") && (await bar()).includes("Later"), await bar());
    await evalJs("[...document.querySelectorAll('#updatebar button')].find(b => b.textContent.includes('new')).click()"); await sleep(200);
    check("update bar: what's new shows the release notes", (await bar()).includes("Faster first start."));
    await state({ phase: "downloading", progress: 42 });
    check("update bar: shows download progress", (await bar()).includes("42%") && (await evalJs("document.querySelector('#updatebar .meter i').style.width")) === "42%");
    await state({ phase: "ready", progress: 100 });
    check("update bar: ready offers the release link without the Android app", (await bar()).includes("ready to install") && (await count("#updatebar a")) === 1);
    await state({ phase: "error", error: "the download did not match its checksum, so it was discarded" });
    check("update bar: an error says Problem and offers Try again", (await bar()).includes("Problem") && (await bar()).includes("Try again") && (await bar()).includes("checksum"));
    await state({ phase: "available", dismissed: true });
    check("update bar: stays hidden once dismissed", await evalJs("document.getElementById('updatebar').hidden"));
    await state({ phase: "available" });
    await go("#/control", 1500);
    check("control: the updates section shows version and the switches", (await text("#control")).includes("Version 0.1.0") && (await text("#control")).includes("Check for updates") && (await text("#control")).includes("Update available"));
    mockUpdate = null;
    await evalJs("location.hash = '#/chat'; location.reload()"); await sleep(1200);
  }
  await go("#/keep-running", 800);
  check(`${theme}: keep-running screen renders three steps`, (await count("#keep .step")) === 3);
  check(`${theme}: keep-running hides the app bar and tabs`, await evalJs("document.getElementById('bar').offsetParent === null && document.getElementById('tabs').offsetParent === null"));
  check(`${theme}: keep-running last step is fully visible`, await evalJs("(() => { const s = document.querySelectorAll('#keep .step'); const c = s[2]; return c.scrollHeight <= c.clientHeight + 1 && c.getBoundingClientRect().height > 60; })()"));
  await shot(`${theme}-7-keep-running`);
}

check("no script errors", problems.length === 0, problems.slice(0, 3).join(" | "));
fs.writeFileSync(path.join(out, "results.json"), JSON.stringify(results, null, 2));
ws.close();
chrome.kill();
const failed = results.filter((r) => !r.ok).length;
console.log(`${results.length - failed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
