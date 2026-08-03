import WebSocket from "ws";
const port = process.argv[2];
const expr = process.argv[3];
const targets = await (await fetch(`http://127.0.0.1:${port}/json`)).json();
const page = targets.find(t => t.type === "page" && t.url.includes("localhost:1420"));
const ws = new WebSocket(page.webSocketDebuggerUrl);
ws.on("open", () => ws.send(JSON.stringify({ id: 1, method: "Runtime.evaluate", params: { expression: expr, returnByValue: true, awaitPromise: true } })));
ws.on("message", (d) => { const m = JSON.parse(d); if (m.id === 1) { console.log(JSON.stringify(m.result?.result?.value)); ws.close(); process.exit(0); } });
setTimeout(() => { console.log("timeout"); process.exit(1); }, 20000);
