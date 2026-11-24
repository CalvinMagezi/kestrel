// Captures the screens used on the website from a running daemon, in dark theme at phone size.
// Usage: node scripts/site-shots.mjs <base-url> <token> <out-dir>
// The daemon should have warmed up and, for the record screen, have a few journal entries.
import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const [base, token, out] = process.argv.slice(2);
if (!base || !token || !out) { console.error("usage: node scripts/site-shots.mjs <base-url> <token> <out-dir>"); process.exit(2); }
fs.mkdirSync(out, { recursive: true });
const CHROME = process.env.CHROME || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const profile = fs.mkdtempSync(path.join(os.tmpdir(), "fx-shots-"));
const chrome = spawn(CHROME, ["--headless=new", "--remote-debugging-port=9555", `--user-data-dir=${profile}`, "--no-first-run", "--disable-gpu", "about:blank"], { stdio: "ignore" });
let ws;
for (let i = 0; i < 50; i++) { try { const l = await (await fetch("http://127.0.0.1:9555/json/list")).json(); const p = l.find((t) => t.type === "page"); if (p) { ws = new WebSocket(p.webSocketDebuggerUrl); break; } } catch {} await sleep(200); }
await new Promise((r) => ws.addEventListener("open", r, { once: true }));
let id = 0; const pend = new Map(); let mockUpdate = null;
ws.addEventListener("message", (m) => {
  const x = JSON.parse(m.data);
  if (x.id && pend.has(x.id)) { pend.get(x.id)(x); pend.delete(x.id); return; }
  if (x.method === "Fetch.requestPaused") {
    const { requestId, request } = x.params;
    if (mockUpdate && request.url.includes("/api/update") && request.method === "GET") {
      send("Fetch.fulfillRequest", { requestId, responseCode: 200, responseHeaders: [{ name: "content-type", value: "application/json" }], body: Buffer.from(JSON.stringify(mockUpdate)).toString("base64") });
    } else send("Fetch.continueRequest", { requestId });
  }
});
const send = (method, params = {}) => new Promise((res, rej) => { const i = ++id; pend.set(i, (m) => (m.error ? rej(new Error(m.error.message)) : res(m.result))); ws.send(JSON.stringify({ id: i, method, params })); });
const ev = async (e) => (await send("Runtime.evaluate", { expression: e, awaitPromise: true, returnByValue: true })).result.value;
const shot = async (name) => { const { data } = await send("Page.captureScreenshot", { format: "png" }); fs.writeFileSync(path.join(out, `${name}.png`), Buffer.from(data, "base64")); console.log("saved", name); };
await send("Page.enable"); await send("Runtime.enable"); await send("Fetch.enable", { patterns: [{ urlPattern: "*api/update*" }] });
await send("Emulation.setDeviceMetricsOverride", { width: 390, height: 844, deviceScaleFactor: 2, mobile: true });
await send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: "dark" }] });
await send("Page.navigate", { url: `${base}/?t=${token}#/chat` }); await sleep(1500);
await ev("localStorage.removeItem('fx-chat'); location.reload()"); await sleep(1500);

await ev("document.getElementById('msg').value='backtest EURUSD'; document.getElementById('form').requestSubmit()"); await sleep(7000);
await shot("chat");

mockUpdate = { phase: "available", current: "0.1.0", progress: 0, error: null, checked_at: Math.floor(Date.now() / 1000), enabled: true, wifi_only: true, dismissed: false,
  release: { version: "0.2.0", notes: "Faster first start.\nA new Learn section on position sizing.", apk_url: "https://github.com/x", sums_url: "https://github.com/x", size: 12582912, page_url: "https://github.com/CalvinMagezi/kestrel/releases/latest" } };
await ev("location.hash = '#/chat'; location.reload()"); await sleep(1800);
await shot("update");
mockUpdate = null;
await ev("location.hash = '#/chat'; location.reload()"); await sleep(1500);

await ev("location.hash = '#/overview'"); await sleep(2500); await shot("overview");
await ev("location.hash = '#/learn'"); await sleep(1500);
await ev("document.querySelector('#learn .calc').scrollIntoView({block:'center'})"); await sleep(500);
await shot("learn");
await ev("location.hash = '#/control'"); await sleep(1500);
await ev("[...document.querySelectorAll('#control .sec')].find(e => e.textContent === 'Your record').scrollIntoView({block:'start'})"); await sleep(600);
await shot("record");
ws.close(); chrome.kill();
